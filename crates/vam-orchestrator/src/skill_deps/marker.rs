//! PEP 508 environment markers, evaluated against the [`Target`]. Pure and fail closed: any
//! variable, operator or syntax this evaluator does not know is an error, never a skipped line.

use super::names::{cmp_release, parse_release};
use super::target::Target;
use std::cmp::Ordering;

/// Why a marker could not be evaluated; `0` is the offending token for the refusal text.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MarkerError(pub String);

/// The values the variables take for the target. `extra` is empty: no extra is ever requested.
pub(crate) struct MarkerEnv {
    python_version: String,
    python_full_version: String,
}

impl MarkerEnv {
    pub(crate) fn new(target: &Target) -> MarkerEnv {
        MarkerEnv {
            python_version: target.python_version(),
            python_full_version: target.python_full_version(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    Word(String),
    Str(String),
    Op(String),
}

fn tokenize(src: &str) -> Result<Vec<Tok>, MarkerError> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => i += 1,
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '\'' | '"' => {
                let mut j = i + 1;
                let mut s = String::new();
                loop {
                    match chars.get(j) {
                        Some(&q) if q == c => break,
                        Some(&ch) => s.push(ch),
                        None => return Err(MarkerError("unterminated string".into())),
                    }
                    j += 1;
                }
                out.push(Tok::Str(s));
                i = j + 1;
            }
            '<' | '>' | '=' | '!' | '~' => {
                let mut j = i;
                while j < chars.len() && matches!(chars[j], '<' | '>' | '=' | '!' | '~') {
                    j += 1;
                }
                out.push(Tok::Op(chars[i..j].iter().collect()));
                i = j;
            }
            c if c.is_ascii_alphanumeric() || c == '_' || c == '.' => {
                let mut j = i;
                while j < chars.len()
                    && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == '.')
                {
                    j += 1;
                }
                out.push(Tok::Word(chars[i..j].iter().collect()));
                i = j;
            }
            other => return Err(MarkerError(other.to_string())),
        }
    }
    Ok(out)
}

/// A side of a comparison: the variable it came from (if any) and its value.
struct Operand {
    var: Option<&'static str>,
    value: String,
}

fn variable(name: &str, env: &MarkerEnv) -> Result<Operand, MarkerError> {
    let (canon, value): (&'static str, String) = match name {
        "python_version" => ("python_version", env.python_version.clone()),
        "python_full_version" => ("python_full_version", env.python_full_version.clone()),
        "sys_platform" | "sys.platform" => ("sys_platform", "linux".into()),
        "platform_system" => ("platform_system", "Linux".into()),
        "os_name" | "os.name" => ("os_name", "posix".into()),
        "platform_machine" | "platform.machine" => ("platform_machine", "x86_64".into()),
        "implementation_name" => ("implementation_name", "cpython".into()),
        "platform_python_implementation"
        | "platform.python_implementation"
        | "python_implementation" => ("platform_python_implementation", "CPython".into()),
        "extra" => ("extra", String::new()),
        other => return Err(MarkerError(other.to_string())),
    };
    Ok(Operand {
        var: Some(canon),
        value,
    })
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    env: &'a MarkerEnv,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(x)) if x == w)
    }

    fn or_expr(&mut self) -> Result<bool, MarkerError> {
        let mut v = self.and_expr()?;
        while self.is_word("or") {
            self.pos += 1;
            let r = self.and_expr()?;
            v = v || r;
        }
        Ok(v)
    }

    fn and_expr(&mut self) -> Result<bool, MarkerError> {
        let mut v = self.atom()?;
        while self.is_word("and") {
            self.pos += 1;
            let r = self.atom()?;
            v = v && r;
        }
        Ok(v)
    }

    fn atom(&mut self) -> Result<bool, MarkerError> {
        if self.peek() == Some(&Tok::LParen) {
            self.pos += 1;
            let v = self.or_expr()?;
            return match self.next() {
                Some(Tok::RParen) => Ok(v),
                _ => Err(MarkerError("unbalanced parenthesis".into())),
            };
        }
        let left = self.operand()?;
        let op = match self.next() {
            Some(Tok::Op(op)) => op,
            Some(Tok::Word(w)) if w == "in" => "in".to_string(),
            Some(Tok::Word(w)) if w == "not" => {
                if self.is_word("in") {
                    self.pos += 1;
                    "not in".to_string()
                } else {
                    return Err(MarkerError("not".into()));
                }
            }
            Some(Tok::Word(w)) => return Err(MarkerError(w)),
            Some(Tok::Str(s)) => return Err(MarkerError(s)),
            _ => return Err(MarkerError("incomplete marker".into())),
        };
        let right = self.operand()?;
        compare(&left, &op, &right)
    }

    fn operand(&mut self) -> Result<Operand, MarkerError> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(Operand {
                var: None,
                value: s,
            }),
            Some(Tok::Word(w)) => variable(&w, self.env),
            Some(Tok::LParen) => Err(MarkerError("(".into())),
            Some(Tok::RParen) => Err(MarkerError(")".into())),
            Some(Tok::Op(o)) => Err(MarkerError(o)),
            None => Err(MarkerError("incomplete marker".into())),
        }
    }
}

fn is_version_var(o: &Operand) -> bool {
    matches!(o.var, Some("python_version" | "python_full_version"))
}

fn compare(l: &Operand, op: &str, r: &Operand) -> Result<bool, MarkerError> {
    match op {
        "in" => return Ok(r.value.contains(&l.value)),
        "not in" => return Ok(!r.value.contains(&l.value)),
        "===" => return Ok(l.value == r.value),
        "<" | "<=" | ">" | ">=" | "==" | "!=" | "~=" => {}
        other => return Err(MarkerError(other.to_string())),
    }
    if is_version_var(l) || is_version_var(r) {
        if let Some(res) = compare_versions(&l.value, op, &r.value) {
            return Ok(res);
        }
        // A version variable against something that is not a version: ordering is undefined.
        return match op {
            "==" => Ok(false),
            "!=" => Ok(true),
            _ => Err(MarkerError(format!("{} {op} {}", l.value, r.value))),
        };
    }
    let ord = l.value.cmp(&r.value);
    match op {
        "==" => Ok(ord == Ordering::Equal),
        "!=" => Ok(ord != Ordering::Equal),
        "<" => Ok(ord == Ordering::Less),
        "<=" => Ok(ord != Ordering::Greater),
        ">" => Ok(ord == Ordering::Greater),
        ">=" => Ok(ord != Ordering::Less),
        other => Err(MarkerError(other.to_string())),
    }
}

fn compare_versions(l: &str, op: &str, r: &str) -> Option<bool> {
    let lv = parse_release(l)?;
    if let Some(prefix) = r.strip_suffix(".*") {
        let rv = parse_release(prefix)?;
        let hit = lv.len() >= rv.len() && lv[..rv.len()] == rv[..];
        return match op {
            "==" => Some(hit),
            "!=" => Some(!hit),
            _ => None,
        };
    }
    let rv = parse_release(r)?;
    let ord = cmp_release(&lv, &rv);
    Some(match op {
        "==" => ord == Ordering::Equal,
        "!=" => ord != Ordering::Equal,
        "<" => ord == Ordering::Less,
        "<=" => ord != Ordering::Greater,
        ">" => ord == Ordering::Greater,
        ">=" => ord != Ordering::Less,
        "~=" => {
            if rv.len() < 2 {
                return None;
            }
            let upper = &rv[..rv.len() - 1];
            ord != Ordering::Less && lv.len() >= upper.len() && lv[..upper.len()] == upper[..]
        }
        _ => return None,
    })
}

/// Evaluate a marker for the target. `Ok(false)` means the requirement does not apply.
pub(crate) fn evaluate(src: &str, env: &MarkerEnv) -> Result<bool, MarkerError> {
    let toks = tokenize(src)?;
    if toks.is_empty() {
        return Err(MarkerError("empty marker".into()));
    }
    let mut p = Parser { toks, pos: 0, env };
    let v = p.or_expr()?;
    if p.pos < p.toks.len() {
        let extra = match &p.toks[p.pos] {
            Tok::Word(w) | Tok::Str(w) | Tok::Op(w) => w.clone(),
            Tok::LParen => "(".to_string(),
            Tok::RParen => ")".to_string(),
        };
        return Err(MarkerError(extra));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> MarkerEnv {
        MarkerEnv::new(&Target {
            python_minor: 13,
            python_patch: 5,
            glibc_minor: 41,
        })
    }

    fn ev(s: &str) -> Result<bool, MarkerError> {
        evaluate(s, &env())
    }

    #[test]
    fn each_variable_true_and_false() {
        for (t, f) in [
            ("python_version == \"3.13\"", "python_version == \"3.12\""),
            (
                "python_full_version == \"3.13.5\"",
                "python_full_version == \"3.13.4\"",
            ),
            ("sys_platform == 'linux'", "sys_platform == 'win32'"),
            ("platform_system == 'Linux'", "platform_system == 'Darwin'"),
            ("os_name == 'posix'", "os_name == 'nt'"),
            (
                "platform_machine == 'x86_64'",
                "platform_machine == 'aarch64'",
            ),
            (
                "implementation_name == 'cpython'",
                "implementation_name == 'pypy'",
            ),
            (
                "platform_python_implementation == 'CPython'",
                "platform_python_implementation == 'PyPy'",
            ),
            ("extra == \"\"", "extra == \"test\""),
        ] {
            assert_eq!(ev(t), Ok(true), "{t}");
            assert_eq!(ev(f), Ok(false), "{f}");
        }
    }

    #[test]
    fn extra_is_never_requested() {
        assert_eq!(ev("extra == 'dev'"), Ok(false));
        assert_eq!(ev("python_version >= '3.8' and extra == 'dev'"), Ok(false));
    }

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(ev("python_version >= '3.9'"), Ok(true));
        assert_eq!(ev("python_version < '3.9'"), Ok(false));
        assert_eq!(ev("python_version > '3.2'"), Ok(true));
        assert_eq!(
            ev("python_version >= '3.10'"),
            Ok(true),
            "3.13 vs 3.10 is not lexical"
        );
        assert_eq!(ev("python_version < '3.100'"), Ok(true));
        assert_eq!(ev("python_version ~= '3.8'"), Ok(true));
        assert_eq!(ev("python_version != '3.13'"), Ok(false));
        assert_eq!(ev("python_version == '3.*'"), Ok(true));
        assert_eq!(ev("python_full_version >= '3.13.5'"), Ok(true));
        assert_eq!(ev("python_full_version > '3.13.5'"), Ok(false));
    }

    #[test]
    fn and_or_parens_precedence() {
        assert_eq!(
            ev("sys_platform == 'win32' or sys_platform == 'linux'"),
            Ok(true)
        );
        assert_eq!(ev("sys_platform == 'linux' and os_name == 'nt'"), Ok(false));
        assert_eq!(
            ev("sys_platform == 'win32' and os_name == 'nt' or sys_platform == 'linux'"),
            Ok(true),
            "and binds tighter than or"
        );
        assert_eq!(
            ev("(sys_platform == 'win32' or sys_platform == 'linux') and os_name == 'nt'"),
            Ok(false)
        );
        assert_eq!(ev("((python_version >= '3.8'))"), Ok(true));
    }

    #[test]
    fn in_and_not_in() {
        assert_eq!(ev("'lin' in sys_platform"), Ok(true));
        assert_eq!(ev("sys_platform in 'linux darwin'"), Ok(true));
        assert_eq!(ev("sys_platform not in 'win32 cygwin'"), Ok(true));
        assert_eq!(ev("sys_platform not in 'linux'"), Ok(false));
    }

    #[test]
    fn legacy_dotted_names() {
        assert_eq!(ev("sys.platform == 'linux'"), Ok(true));
        assert_eq!(ev("os.name == 'posix'"), Ok(true));
        assert_eq!(ev("platform.machine == 'x86_64'"), Ok(true));
        assert_eq!(ev("python_implementation == 'CPython'"), Ok(true));
    }

    #[test]
    fn unknown_variable_is_refused_not_skipped() {
        for s in [
            "platform_release == '6.1'",
            "implementation_version == '3.13'",
            "platform_version == 'x'",
            "bogus == 'x'",
            "sys_platform == 'linux' and bogus == 'x'",
        ] {
            assert!(ev(s).is_err(), "{s}");
        }
        assert_eq!(
            ev("platform_release == '6.1'"),
            Err(MarkerError("platform_release".into()))
        );
    }

    #[test]
    fn syntax_errors_are_refused() {
        for s in [
            "",
            "(sys_platform == 'linux'",
            "sys_platform == 'linux')",
            "sys_platform ==",
            "sys_platform 'linux'",
            "sys_platform == 'linux",
            "sys_platform === 'linux' extra",
            "sys_platform <> 'linux'",
            "sys_platform == 'linux' and",
            "sys_platform == 'linux' $",
        ] {
            assert!(ev(s).is_err(), "{s:?}");
        }
    }
}
