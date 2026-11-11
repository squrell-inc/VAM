//! Compact `INDEX.md` contract for large skill `docs/` trees.
//!
//! Location: `/mnt/drive9/skills/<skill-id>/INDEX.md` (skill package root).
//! Optional: packages without an index continue to load unchanged.

use crate::skills::SKILLS_MOUNT;

pub const INDEX_FILENAME: &str = "INDEX.md";

const HEADER: &str = "\
# VAM skill documentation index
#
# Tab-separated `name<TAB>relative_path` rows follow this header.
# Paths are relative to the skill package root (the directory that
# contains SKILLS.md). Names are unique. Duplicate names keep the
# first path and suffix later names with ` (2)`, ` (3)`, and so on.
# Rows are sorted by name (Unicode code point / UTF-8 byte order).
# TAB, newline, carriage return, and backslash in names or paths
# are escaped as `\\t`, `\\n`, `\\r`, `\\\\`.
#
# Agents: grep this file by logical table or document name, then
# read_file `/mnt/drive9/skills/<skill-id>/<relative_path>`.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub relative_path: String,
}

/// Concise SKILLS.md guidance: grep-by-name, then read the absolute guest path.
pub fn skills_md_index_guidance(skill_id: &str) -> String {
    format!(
        "Large docs: grep INDEX.md by table or document name with path `/mnt/drive9/skills/{skill_id}`, then read_file `/mnt/drive9/skills/{skill_id}/<relative-path-from-index>`. Do not glob the docs tree."
    )
}

pub fn guest_index_path(skill_id: &str) -> String {
    format!("{SKILLS_MOUNT}/{skill_id}/{INDEX_FILENAME}")
}

pub fn guest_doc_path(skill_id: &str, relative_path: &str) -> String {
    let rel = relative_path.trim_start_matches('/');
    format!("{SKILLS_MOUNT}/{skill_id}/{rel}")
}

/// Deterministic INDEX.md body: unique names, escaped fields, sorted rows.
pub fn generate_index(entries: impl IntoIterator<Item = (String, String)>) -> String {
    let uniqued = uniquify_names(entries);
    let mut rows: Vec<(String, String)> = uniqued
        .into_iter()
        .filter(|(_, path)| is_safe_relative_path(path))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::from(HEADER);
    out.push('\n');
    for (name, path) in rows {
        out.push_str(&escape_field(&name));
        out.push('\t');
        out.push_str(&escape_field(&path));
        out.push('\n');
    }
    out
}

pub fn parse_index(content: &str) -> Result<Vec<IndexEntry>, String> {
    let mut entries = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name_raw, path_raw)) = line.split_once('\t') else {
            return Err(format!("INDEX.md line {} is not tab-separated", idx + 1));
        };
        let name = unescape_field(name_raw);
        let relative_path = unescape_field(path_raw);
        if name.is_empty() || relative_path.is_empty() {
            return Err(format!(
                "INDEX.md line {} has an empty name or path",
                idx + 1
            ));
        }
        if !is_safe_relative_path(&relative_path) {
            return Err(format!(
                "INDEX.md line {} path '{}' is not a safe relative path",
                idx + 1,
                relative_path
            ));
        }
        entries.push(IndexEntry {
            name,
            relative_path,
        });
    }
    Ok(entries)
}

pub fn lookup<'a>(entries: &'a [IndexEntry], name: &str) -> Option<&'a IndexEntry> {
    entries.iter().find(|e| e.name == name)
}

fn uniquify_names(entries: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for (name, path) in entries {
        let count = seen.entry(name.clone()).or_insert(0);
        *count += 1;
        let unique = if *count == 1 {
            name
        } else {
            format!("{name} ({count})")
        };
        out.push((unique, path));
    }
    out
}

fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\0') {
        return false;
    }
    for component in path.split(['/', '\\']) {
        if component == ".." {
            return false;
        }
    }
    true
}

fn escape_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

fn unescape_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_is_sorted_unique_and_escaped() {
        let index = generate_index([
            ("zeta".into(), "docs/z.md".into()),
            ("alpha".into(), "docs/a.md".into()),
            ("alpha".into(), "docs/a2.md".into()),
            ("has\ttab".into(), "docs/tab.md".into()),
        ]);
        let parsed = parse_index(&index).expect("parse");
        let names: Vec<&str> = parsed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "alpha (2)", "has\ttab", "zeta"]);
        assert_eq!(parsed[1].relative_path, "docs/a2.md");
        assert!(index.contains("has\\ttab\tdocs/tab.md"));
        let alpha = lookup(&parsed, "alpha").expect("alpha");
        assert_eq!(alpha.relative_path, "docs/a.md");
    }

    #[test]
    fn rejects_parent_dir_and_absolute_paths() {
        let index = generate_index([
            ("ok".into(), "docs/ok.md".into()),
            ("escape".into(), "../secret.md".into()),
            ("abs".into(), "/etc/passwd".into()),
        ]);
        let parsed = parse_index(&index).expect("parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "ok");
    }

    #[test]
    fn guest_paths_are_absolute_skills_mount() {
        assert_eq!(
            guest_index_path("oracle-sql"),
            "/mnt/drive9/skills/oracle-sql/INDEX.md"
        );
        assert_eq!(
            guest_doc_path("oracle-sql", "docs/tables/po_headers_all.md"),
            "/mnt/drive9/skills/oracle-sql/docs/tables/po_headers_all.md"
        );
    }

    #[test]
    fn large_synthetic_index_exceeds_grep_hit_cap() {
        let entries: Vec<(String, String)> = (0..250)
            .map(|i| {
                (
                    format!("table_{i:04}"),
                    format!("docs/tables/table_{i:04}.md"),
                )
            })
            .collect();
        let index = generate_index(entries);
        let parsed = parse_index(&index).expect("parse");
        assert_eq!(parsed.len(), 250);
        let hit = lookup(&parsed, "table_0249").expect("last");
        assert_eq!(
            guest_doc_path("oracle-sql", &hit.relative_path),
            "/mnt/drive9/skills/oracle-sql/docs/tables/table_0249.md"
        );
        assert!(skills_md_index_guidance("oracle-sql").contains("grep INDEX.md"));
        assert!(skills_md_index_guidance("oracle-sql").contains("Do not glob the docs tree"));
    }
}
