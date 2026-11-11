//! One redactor for every place a secret could reach the guest.
//!
//! A host tool, an MCP or hosted broker and a vendor's response may echo a credential in
//! more shapes than the raw value. [`SecretRedactor`] removes all of these from a byte
//! string, in one pass:
//!
//! - the raw value, whatever its length;
//! - base64 (standard and URL alphabets, padded and not) of the value alone, and of the
//!   value inside a larger blob at each of the three byte alignments (HTTP Basic auth is
//!   `base64("user:" + secret)`), trimmed to the characters the neighbours cannot change;
//! - hex (lower and upper), percent-encoded (reserved bytes only, every byte, `+` for a
//!   space) and JSON-escaped;
//! - any of the above split by whitespace (a line break, a space, a CRLF, base64 wrapped at
//!   76 columns), for needles of [`MIN_PARTIAL_RUN`] bytes or more;
//! - any run of [`MIN_PARTIAL_RUN`] or more consecutive bytes of the raw value.
//!
//! A secret shorter than [`MIN_PARTIAL_RUN`] bytes is redacted whole and in its encoded
//! forms only, and exactly: no whitespace tolerance, no partial run. An encoded form shorter
//! than [`MIN_ENCODED_NEEDLE`] bytes is not searched for if it is base64 or hex (it would
//! match ordinary text); a percent or JSON form is always exact.
//!
//! The redactor works on bytes: a binary artifact is returned intact apart from the
//! redacted runs. Overlapping and touching matches merge into one replacement.
//!
//! Cost: building is linear in the needle bytes (about 30 times the secret's length, plus
//! one 12-byte window per secret byte); a scan is one Aho-Corasick pass, O(text + matches),
//! plus a second pass over a whitespace-stripped copy of the text (4 extra bytes per text
//! byte) only when the text holds whitespace.
//!
//! Not covered: an encoded form's partial runs, reversed, XOR-ed or multiply-encoded
//! values, and compressed text. Nothing here ever prints a secret.

use std::borrow::Cow;
use std::collections::HashSet;

use aho_corasick::{AhoCorasick, MatchKind};

/// The shortest run of consecutive secret bytes that is redacted on its own.
pub const MIN_PARTIAL_RUN: usize = 12;
/// The text that replaces a redacted run unless the caller picks another.
pub const DEFAULT_REPLACEMENT: &str = "***";
/// An encoded form shorter than this is not searched for.
pub const MIN_ENCODED_NEEDLE: usize = 8;
/// A 12-byte window with fewer distinct bytes than this ("aaaaaaaaaaaa") is not indexed.
const MIN_DISTINCT_BYTES: usize = 4;
/// Windows are taken over at most this many leading bytes of a secret.
const MAX_WINDOW_SPAN: usize = 16 * 1024;

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn strip_ws(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().copied().filter(|b| !is_ws(*b)).collect()
}

fn base64(data: &[u8], url: bool, pad: bool) -> Vec<u8> {
    let alphabet = if url { URL } else { STD };
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(alphabet[(n >> 18) as usize & 63]);
        out.push(alphabet[(n >> 12) as usize & 63]);
        if chunk.len() > 1 {
            out.push(alphabet[(n >> 6) as usize & 63]);
        } else if pad {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[n as usize & 63]);
        } else if pad {
            out.push(b'=');
        }
    }
    out
}

fn hex(data: &[u8], upper: bool) -> Vec<u8> {
    let digits: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut out = Vec::with_capacity(data.len() * 2);
    for b in data {
        out.push(digits[(b >> 4) as usize]);
        out.push(digits[(b & 15) as usize]);
    }
    out
}

/// Percent-encoding. `all` encodes every byte; otherwise RFC 3986 unreserved bytes stay.
/// `plus_for_space` is the form-encoding of a space.
fn percent(data: &[u8], upper: bool, all: bool, plus_for_space: bool) -> Vec<u8> {
    let digits: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut out = Vec::with_capacity(data.len() * 3);
    for &b in data {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if plus_for_space && b == b' ' && !all {
            out.push(b'+');
        } else if unreserved && !all {
            out.push(b);
        } else {
            out.push(b'%');
            out.push(digits[(b >> 4) as usize]);
            out.push(digits[(b & 15) as usize]);
        }
    }
    out
}

/// Every searchable form of `secret` as `(form name, bytes)`. The raw value comes first.
/// Names are for tests and messages; they never carry the value.
pub fn forms_of(secret: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    let mut forms: Vec<(&'static str, Vec<u8>)> = vec![("raw", secret.to_vec())];
    for (name, url) in [("base64", false), ("base64url", true)] {
        for pad in [true, false] {
            forms.push((name, base64(secret, url, pad)));
        }
        // The value inside a larger blob: `k` bytes before it. Characters that mix in the
        // bytes before (the first ceil(8k/6)) or after (the last partial one) are left out.
        for k in 0..3usize {
            let mut blob = vec![0u8; k];
            blob.extend_from_slice(secret);
            let encoded = base64(&blob, url, false);
            let skip = (8 * k).div_ceil(6);
            let end = 8 * (k + secret.len()) / 6;
            if skip < end && end <= encoded.len() {
                forms.push((name, encoded[skip..end].to_vec()));
            }
        }
    }
    forms.push(("hex", hex(secret, false)));
    forms.push(("hex", hex(secret, true)));
    for upper in [true, false] {
        forms.push(("percent", percent(secret, upper, false, false)));
        forms.push(("percent", percent(secret, upper, true, false)));
        if secret.contains(&b' ') {
            forms.push(("percent", percent(secret, upper, false, true)));
        }
    }
    if let Ok(text) = std::str::from_utf8(secret) {
        if let Ok(quoted) = serde_json::to_string(text) {
            if let Some(inner) = quoted.strip_prefix('"').and_then(|q| q.strip_suffix('"')) {
                forms.push(("json", inner.as_bytes().to_vec()));
            }
        }
    }
    let mut seen = HashSet::new();
    forms.retain(|(name, bytes)| {
        // Percent and JSON forms carry `%` or `\` where they differ from the raw value, so
        // even a short one is exact. Base64 and hex of a short value look like ordinary text.
        let exact_form = matches!(*name, "raw" | "json" | "percent");
        let long_enough = exact_form || bytes.len() >= MIN_ENCODED_NEEDLE;
        !bytes.is_empty() && long_enough && seen.insert(bytes.clone())
    });
    forms
}

fn push_windows(bytes: &[u8], out: &mut Vec<Vec<u8>>) {
    if bytes.len() <= MIN_PARTIAL_RUN {
        return;
    }
    let span = bytes.len().min(MAX_WINDOW_SPAN + MIN_PARTIAL_RUN - 1);
    for window in bytes[..span].windows(MIN_PARTIAL_RUN) {
        // A run that starts or ends on whitespace ("Bearer token" cut at the space) would
        // eat the separator of ordinary text next to a match; its neighbours cover the rest.
        if is_ws(window[0]) || is_ws(window[MIN_PARTIAL_RUN - 1]) {
            continue;
        }
        let mut seen = [false; 256];
        let mut distinct = 0;
        for &b in window {
            if !seen[b as usize] {
                seen[b as usize] = true;
                distinct += 1;
            }
        }
        if distinct >= MIN_DISTINCT_BYTES {
            out.push(window.to_vec());
        }
    }
}

fn dedup(mut patterns: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut seen = HashSet::new();
    patterns.retain(|p| !p.is_empty() && seen.insert(p.clone()));
    patterns
}

fn build(patterns: &[Vec<u8>]) -> Option<AhoCorasick> {
    if patterns.is_empty() {
        return None;
    }
    // Overlapping search needs the standard match kind; it reports every needle at every
    // position, so a run longer than a window is covered window by window and merged.
    AhoCorasick::builder()
        .match_kind(MatchKind::Standard)
        .build(patterns)
        .ok()
}

fn merge(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Removes every form of a set of secrets from text and bytes. See the module docs.
pub struct SecretRedactor {
    exact: Option<AhoCorasick>,
    tolerant: Option<AhoCorasick>,
    /// Whole needles, searched by hand if the automaton could not be built.
    whole: Vec<Vec<u8>>,
    replacement: Vec<u8>,
}

impl SecretRedactor {
    /// A redactor for `secrets` that replaces each run with `***`. Empty values are skipped.
    pub fn new<S: AsRef<[u8]>>(secrets: &[S]) -> Self {
        Self::with_replacement(secrets, DEFAULT_REPLACEMENT)
    }

    pub fn with_replacement<S: AsRef<[u8]>>(secrets: &[S], replacement: &str) -> Self {
        let mut exact: Vec<Vec<u8>> = Vec::new();
        let mut tolerant: Vec<Vec<u8>> = Vec::new();
        let mut whole: Vec<Vec<u8>> = Vec::new();
        for secret in secrets.iter().map(AsRef::as_ref).filter(|s| !s.is_empty()) {
            for (_, needle) in forms_of(secret) {
                let stripped = strip_ws(&needle);
                if stripped.len() >= MIN_PARTIAL_RUN {
                    tolerant.push(stripped);
                }
                whole.push(needle.clone());
                exact.push(needle);
            }
            push_windows(secret, &mut exact);
            let stripped = strip_ws(secret);
            push_windows(&stripped, &mut tolerant);
        }
        let exact = dedup(exact);
        let tolerant = dedup(tolerant);
        Self {
            exact: build(&exact),
            tolerant: build(&tolerant),
            whole: dedup(whole),
            replacement: replacement.as_bytes().to_vec(),
        }
    }

    /// True when there is nothing to redact (no non-empty secret).
    pub fn is_empty(&self) -> bool {
        self.exact.is_none() && self.whole.is_empty()
    }

    fn ranges(&self, input: &[u8]) -> Vec<(usize, usize)> {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        match &self.exact {
            Some(ac) => {
                ranges.extend(
                    ac.find_overlapping_iter(input)
                        .map(|m| (m.start(), m.end())),
                );
            }
            None => {
                for needle in &self.whole {
                    let mut from = 0;
                    while let Some(at) = find(&input[from..], needle) {
                        ranges.push((from + at, from + at + needle.len()));
                        from += at + 1;
                    }
                }
            }
        }
        if let Some(ac) = &self.tolerant {
            if input.len() <= u32::MAX as usize && input.iter().any(|b| is_ws(*b)) {
                let mut view = Vec::with_capacity(input.len());
                let mut origin: Vec<u32> = Vec::with_capacity(input.len());
                for (at, &b) in input.iter().enumerate() {
                    if !is_ws(b) {
                        view.push(b);
                        origin.push(at as u32);
                    }
                }
                for m in ac.find_overlapping_iter(&view) {
                    ranges.push((origin[m.start()] as usize, origin[m.end() - 1] as usize + 1));
                }
            }
        }
        merge(ranges)
    }

    /// `input` with every redacted run replaced. Borrowed when nothing matched.
    pub fn redact_bytes<'a>(&self, input: &'a [u8]) -> Cow<'a, [u8]> {
        let ranges = self.ranges(input);
        if ranges.is_empty() {
            return Cow::Borrowed(input);
        }
        let mut out = Vec::with_capacity(input.len());
        let mut at = 0;
        for (start, end) in ranges {
            out.extend_from_slice(&input[at..start]);
            out.extend_from_slice(&self.replacement);
            at = end;
        }
        out.extend_from_slice(&input[at..]);
        Cow::Owned(out)
    }

    /// [`Self::redact_bytes`] for text. A run that starts or ends inside a multi-byte
    /// character takes the whole character, so the result is valid UTF-8.
    pub fn redact_str<'a>(&self, input: &'a str) -> Cow<'a, str> {
        let ranges = self.ranges(input.as_bytes());
        if ranges.is_empty() {
            return Cow::Borrowed(input);
        }
        let widened = ranges
            .into_iter()
            .map(|(mut start, mut end)| {
                while !input.is_char_boundary(start) {
                    start -= 1;
                }
                while !input.is_char_boundary(end) {
                    end += 1;
                }
                (start, end)
            })
            .collect();
        let mut out = String::with_capacity(input.len());
        let mut at = 0;
        for (start, end) in merge(widened) {
            out.push_str(&input[at..start]);
            out.push_str(&String::from_utf8_lossy(&self.replacement));
            at = end;
        }
        out.push_str(&input[at..]);
        Cow::Owned(out)
    }

    /// The names of the forms of `secret` found in `input`: for tests and messages, never
    /// the bytes.
    pub fn leaked_forms(secret: &[u8], input: &[u8]) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = Vec::new();
        for (name, needle) in forms_of(secret) {
            if find(input, &needle).is_some() && !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    use base64::Engine as _;

    /// A generated value (never a literal credential): letters and digits, plus the bytes
    /// that percent and JSON encodings change.
    fn secret(seed: u64, len: usize) -> String {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                alphabet[(state % alphabet.len() as u64) as usize] as char
            })
            .collect()
    }

    fn special(seed: u64) -> String {
        format!("tok-{}/+=&\"z", secret(seed, 24))
    }

    /// True when any form, or any 12-byte run, of `value` is still in `text`.
    fn leaks(value: &str, text: &[u8]) -> bool {
        if !SecretRedactor::leaked_forms(value.as_bytes(), text).is_empty() {
            return true;
        }
        value
            .as_bytes()
            .windows(MIN_PARTIAL_RUN)
            .any(|w| find(text, w).is_some())
    }

    fn redacted(value: &str, text: &str) -> String {
        SecretRedactor::new(&[value]).redact_str(text).into_owned()
    }

    #[test]
    fn every_form_is_removed_alone_and_embedded() {
        let value = special(1);
        let hex_lower: String = value.bytes().map(|b| format!("{b:02x}")).collect();
        let encoded: Vec<(&str, String)> = vec![
            ("raw", value.clone()),
            ("base64", STANDARD.encode(&value)),
            ("base64 unpadded", STANDARD_NO_PAD.encode(&value)),
            ("base64url", URL_SAFE.encode(&value)),
            ("base64url unpadded", URL_SAFE_NO_PAD.encode(&value)),
            ("hex lower", hex_lower.clone()),
            ("hex upper", hex_lower.to_uppercase()),
            (
                "percent",
                String::from_utf8(percent(value.as_bytes(), true, false, false)).unwrap(),
            ),
            (
                "percent lower",
                String::from_utf8(percent(value.as_bytes(), false, false, false)).unwrap(),
            ),
            (
                "json",
                serde_json::to_string(&value)
                    .unwrap()
                    .trim_matches('"')
                    .to_string(),
            ),
        ];
        for (form, text) in &encoded {
            let out = redacted(&value, &format!("before {text} after"));
            assert!(!leaks(&value, out.as_bytes()), "form {form} survived");
            assert!(
                out.starts_with("before ") && out.ends_with(" after"),
                "{form}"
            );
            assert!(
                out.contains(DEFAULT_REPLACEMENT),
                "form {form} not replaced"
            );
        }
    }

    #[test]
    fn a_secret_inside_a_larger_base64_blob_is_removed_at_every_alignment() {
        let value = special(2);
        for prefix in ["", "a", "us", "user:", "Basic user:", "x"] {
            for suffix in ["", "!", "tail"] {
                let blob = format!("{prefix}{value}{suffix}");
                for (form, text) in [
                    ("base64", STANDARD.encode(&blob)),
                    ("base64 unpadded", STANDARD_NO_PAD.encode(&blob)),
                    ("base64url", URL_SAFE.encode(&blob)),
                    ("base64url unpadded", URL_SAFE_NO_PAD.encode(&blob)),
                ] {
                    let out = redacted(&value, &format!("Authorization: {text}\n"));
                    // The characters that depend on the neighbours may stay; the ones the
                    // secret alone decides may not. Independent check: any 4-char group
                    // fully inside the secret must be gone.
                    let k = prefix.len();
                    let enc = base64_chars(&blob, form);
                    let skip = (8 * k).div_ceil(6);
                    let end = 8 * (k + value.len()) / 6;
                    let decided = &enc[skip..end];
                    assert!(
                        !out.contains(decided),
                        "form {form} prefix {k} suffix {} survived",
                        suffix.len()
                    );
                }
            }
        }
    }

    fn base64_chars(blob: &str, form: &str) -> String {
        match form {
            "base64" | "base64 unpadded" => STANDARD_NO_PAD.encode(blob),
            _ => URL_SAFE_NO_PAD.encode(blob),
        }
    }

    #[test]
    fn a_secret_split_by_whitespace_is_removed() {
        let value = special(3);
        let mid = value.len() / 2;
        for sep in ["\n", " ", "\r\n", "\t", "  \n  "] {
            let text = format!("x {}{sep}{} y", &value[..mid], &value[mid..]);
            let out = redacted(&value, &text);
            assert!(!leaks(&value, out.as_bytes()), "split by {sep:?} survived");
            assert!(out.starts_with("x ") && out.ends_with(" y"));
        }
    }

    #[test]
    fn base64_wrapped_at_a_column_is_removed() {
        let value = special(4);
        let encoded = STANDARD.encode(format!("user:{value}"));
        let wrapped: Vec<&str> = encoded
            .as_bytes()
            .chunks(10)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        let text = wrapped.join("\n");
        let out = redacted(&value, &text);
        let squeezed: String = out.chars().filter(|c| !c.is_whitespace()).collect();
        let decided = &STANDARD_NO_PAD.encode(format!("user:{value}"))[7..40];
        assert!(!squeezed.contains(decided), "wrapped base64 survived");
    }

    #[test]
    fn a_partial_run_of_twelve_bytes_or_more_is_removed_and_eleven_is_not() {
        let value = secret(5, 40);
        let bytes = value.as_bytes();
        let run15 = &value[7..22];
        let out = redacted(&value, &format!("<{run15}>"));
        assert_eq!(out, "<***>", "a 15-byte run leaves no tail");
        let run12 = &value[0..12];
        assert_eq!(redacted(&value, &format!("<{run12}>")), "<***>");
        let run11 = &value[0..11];
        assert_eq!(
            redacted(&value, &format!("<{run11}>")),
            format!("<{run11}>")
        );
        assert!(bytes.len() >= 40);
    }

    #[test]
    fn a_short_secret_is_handled_exactly() {
        let value = secret(6, 4);
        // Whole value, anywhere, even inside a longer word.
        assert_eq!(redacted(&value, &format!("a{value}b")), "a***b");
        // Not a prefix, not with whitespace in the middle (no tolerance below 12 bytes).
        assert_eq!(redacted(&value, &value[..3]), value[..3]);
        let spaced = format!("{} {}", &value[..2], &value[2..]);
        assert_eq!(redacted(&value, &spaced), spaced);
        // Encoded forms of eight characters or more are still found.
        let b64 = STANDARD.encode(&value);
        assert_eq!(redacted(&value, &format!("[{b64}]")), "[***]");
        let hexed: String = value.bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(redacted(&value, &format!("[{hexed}]")), "[***]");
    }

    #[test]
    fn an_eleven_byte_secret_is_whole_only() {
        let value = secret(7, 11);
        assert_eq!(redacted(&value, &format!("x{value}x")), "x***x");
        let half = &value[..8];
        assert_eq!(redacted(&value, half), half);
        let spaced = format!("{}\n{}", &value[..5], &value[5..]);
        assert_eq!(redacted(&value, &spaced), spaced);
    }

    #[test]
    fn ordinary_text_is_untouched() {
        let value = special(8);
        let prose = "The quick brown fox jumps over the lazy dog.\n\
             Authorization: Bearer redacted-by-someone-else\n\
             https://example.com/api/v1/items?id=12345&sort=asc\n\
             {\"name\": \"report\", \"rows\": [1, 2, 3], \"ok\": true}\n\
             fn main() { println!(\"hello\"); }\n";
        assert_eq!(redacted(&value, prose), prose);
        let other = special(9);
        assert_eq!(redacted(&value, &other), other);
        let text = STANDARD.encode(&other);
        assert_eq!(redacted(&value, &text), text);
    }

    #[test]
    fn a_window_of_one_repeated_byte_is_not_indexed_but_the_whole_value_is() {
        let value = format!("{}XYZ!", "a".repeat(20));
        let filler = "a".repeat(30);
        assert_eq!(redacted(&value, &filler), filler);
        assert_eq!(redacted(&value, &format!("<{value}>")), "<***>");
    }

    #[test]
    fn binary_bytes_stay_intact_around_a_redacted_run() {
        let value = secret(10, 30);
        let mut data = vec![0xff, 0x00, 0xfe, 0x80, 0x01];
        data.extend_from_slice(value.as_bytes());
        data.extend_from_slice(&[0xc3, 0x28, 0x00, 0xff]);
        let out = SecretRedactor::new(&[&value])
            .redact_bytes(&data)
            .into_owned();
        let mut want = vec![0xff, 0x00, 0xfe, 0x80, 0x01];
        want.extend_from_slice(b"***");
        want.extend_from_slice(&[0xc3, 0x28, 0x00, 0xff]);
        assert_eq!(out, want);
        // No match: borrowed, the same bytes.
        let clean = [0u8, 1, 2, 255];
        assert!(matches!(
            SecretRedactor::new(&[&value]).redact_bytes(&clean),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn text_stays_valid_utf8_when_a_run_starts_inside_a_character() {
        // U+4E2D (e4 b8 ad) in the secret, U+5E2D (e5 b8 ad) in the text: a window that
        // starts at the shared continuation bytes matches inside the text's character.
        let tail = secret(17, 8);
        let value = format!("\u{4e2d}\u{6587}{tail}");
        let text = format!("\u{5e2d}\u{6587}{tail}x");
        assert_eq!(redacted(&value, &text), "***x");
    }

    #[test]
    fn overlapping_and_touching_matches_become_one_replacement() {
        let a = secret(11, 20);
        let b = secret(12, 20);
        let redactor = SecretRedactor::new(&[&a, &b]);
        assert_eq!(redactor.redact_str(&format!("{a}{b}")), "***");
        assert_eq!(redactor.redact_str(&format!("{a} {b}")), "*** ***");
        let overlap = format!("{}{}", &a[..16], &a[4..]);
        assert_eq!(redactor.redact_str(&overlap), "***");
    }

    #[test]
    fn several_secrets_and_a_custom_replacement() {
        let a = special(13);
        let b = secret(14, 16);
        let redactor = SecretRedactor::with_replacement(&[&a, &b], "{{SECRET}}");
        let text = format!("one {} two {} three", STANDARD.encode(&a), b);
        assert_eq!(
            redactor.redact_str(&text),
            "one {{SECRET}} two {{SECRET}} three"
        );
    }

    #[test]
    fn a_secret_with_a_space_leaves_the_separators_of_the_text_around_it() {
        let a = "Bearer expired-access";
        let b = "Bearer fresh-access";
        let redactor = SecretRedactor::new(&[a, b, "expired-access", "fresh-access"]);
        assert_eq!(
            redactor.redact_str("create_issue expired-access fresh-access"),
            "create_issue *** ***"
        );
    }

    #[test]
    fn no_secret_means_no_change() {
        let none: [&str; 0] = [];
        let redactor = SecretRedactor::new(&none);
        assert!(redactor.is_empty());
        assert_eq!(redactor.redact_str("anything"), "anything");
        let empty = SecretRedactor::new(&[""]);
        assert!(empty.is_empty());
        assert_eq!(empty.redact_str("anything"), "anything");
    }

    #[test]
    fn a_large_secret_and_a_large_text_stay_linear() {
        let value = secret(15, 20 * 1024);
        let line = "bulk-output-line 0123456789abcdef0123456789abcdef\n";
        let mut text = line.repeat(4000);
        text.push_str(&format!("[{value}]"));
        text.push_str(&line.repeat(1000));
        let redactor = SecretRedactor::new(&[&value]);
        let out = redactor.redact_str(&text);
        assert!(!out.contains(&value[..64]));
        assert!(out.contains("[***]"));
        assert!(out.len() < text.len());
    }

    #[test]
    fn forms_of_never_includes_a_form_shorter_than_the_floor() {
        for (name, needle) in forms_of(secret(16, 4).as_bytes()) {
            if matches!(name, "base64" | "base64url" | "hex") {
                assert!(needle.len() >= MIN_ENCODED_NEEDLE, "form {name} too short");
            }
        }
    }
}
