//! Host-held context set and source list for a task (AR-1.5).
//!
//! Two additive SQLite tables, both keyed by run id and neither holding page
//! text or snippets:
//!
//! * `run_context_urls`: every URL the task has been shown (its prompt, the
//!   tool results vamd returned, a `redirect_to` target). Wave 4 reads it to
//!   decide whether `web_fetch` may take a URL.
//! * `run_sources`: the `S<n>` list. Each source a broker reports gets a
//!   stable id per task (the same normalized URL keeps its id). Only URL,
//!   title, date, kind and the saved file name are stored. `ResultSource` has
//!   no snippet field and no column here could hold one.

use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// URLs held per task. At the cap new URLs are dropped; none is evicted.
pub const MAX_CONTEXT_URLS_PER_RUN: usize = 5000;
/// URLs taken from one tool result.
pub const MAX_CONTEXT_URLS_PER_RESULT: usize = 500;
/// Sources numbered per task.
pub const MAX_SOURCES_PER_RUN: usize = 1000;
/// Sources taken from one tool result.
pub const MAX_SOURCES_PER_CALL: usize = 50;
/// Longest URL kept, as `web_fetch` accepts.
pub const MAX_URL_CHARS: usize = 2048;
const MAX_TITLE_CHARS: usize = 300;
const MAX_PUBLISHED_CHARS: usize = 64;
const MAX_RELPATH_CHARS: usize = 96;

/// Where a context URL came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextOrigin {
    Prompt,
    ToolResult,
    Redirect,
    /// Inherited from an earlier run whose records are in the thread context.
    PriorRun,
}

impl ContextOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::ToolResult => "tool_result",
            Self::Redirect => "redirect",
            Self::PriorRun => "prior_run",
        }
    }
}

/// What kind of source a broker reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    SearchResult,
    Page,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SearchResult => "search_result",
            Self::Page => "page",
        }
    }

    fn parse(raw: &str) -> Self {
        if raw == "page" {
            Self::Page
        } else {
            Self::SearchResult
        }
    }

    /// From the proto enum value; unspecified counts as a search result.
    pub fn from_proto(kind: i32) -> Self {
        if kind == vam_spec::v1::ResultSourceKind::Page as i32 {
            Self::Page
        } else {
            Self::SearchResult
        }
    }
}

/// One source as a broker reported it, already redacted.
#[derive(Debug, Clone)]
pub struct SourceInput {
    pub url: String,
    pub title: String,
    pub published: String,
    pub kind: SourceKind,
    pub artifact_relpath: String,
}

/// A numbered source of a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSource {
    pub n: i64,
    pub url: String,
    pub title: String,
    pub published: String,
    pub retrieved_at: i64,
    pub kind: SourceKind,
    pub artifact_relpath: String,
}

impl RunSource {
    /// The id the model cites: `S<n>`.
    pub fn id(&self) -> String {
        format!("S{}", self.n)
    }
}

/// The one URL normalizer for the context set and for Wave 4's check:
/// http or https only, no userinfo, lowercase scheme and host, default port
/// and fragment dropped, path and query kept exactly. `None` for anything
/// else or a URL over [`MAX_URL_CHARS`].
pub fn normalize_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_URL_CHARS {
        return None;
    }
    let mut url = url::Url::parse(trimmed).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.host_str()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    url.set_fragment(None);
    let out = url.to_string();
    (out.len() <= MAX_URL_CHARS).then_some(out)
}

/// http(s) URLs in free text, normalized, deduplicated, in order of
/// appearance, at most `limit`. A trailing `)` or `]` belongs to the URL only
/// when the URL has a matching opener.
pub fn extract_urls(text: &str, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut from = 0usize;
    while out.len() < limit {
        let Some(rel) = lower[from..].find("http") else {
            break;
        };
        let start = from + rel;
        from = start + 4;
        let rest = &lower[start..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            continue;
        }
        let tail = &text[start..];
        let end = tail
            .find(|c: char| {
                c.is_whitespace()
                    || c.is_control()
                    || matches!(
                        c,
                        '"' | '\'' | '<' | '>' | '`' | '\\' | '|' | '^' | '{' | '}'
                    )
            })
            .unwrap_or(tail.len());
        let mut candidate = &tail[..end];
        loop {
            let before = candidate.len();
            candidate = candidate.trim_end_matches(['.', ',', ';', ':', '!', '?', '*']);
            for (open, close) in [('(', ')'), ('[', ']')] {
                if candidate.ends_with(close)
                    && candidate.matches(close).count() > candidate.matches(open).count()
                {
                    candidate = &candidate[..candidate.len() - 1];
                }
            }
            if candidate.len() == before {
                break;
            }
        }
        from = start + end.max(4);
        if let Some(norm) = normalize_url(candidate) {
            if !out.contains(&norm) {
                out.push(norm);
            }
        }
    }
    out
}

/// One line, control characters dropped, cut to `max` characters.
fn one_line(raw: &str, max: usize) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    let squeezed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    squeezed.chars().take(max).collect()
}

/// A broker-supplied file name reduced to a flat, safe name.
pub fn sanitize_relpath(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(MAX_RELPATH_CHARS)
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "artifact.bin".to_string()
    } else {
        cleaned
    }
}

/// Merge the numbered sources into the broker's output as
/// `sources: [{id, title, url, published?}]`. A non-object output is wrapped
/// as `{"result": <output>, "sources": [...]}`. The result stays JSON, so a
/// playbook step can still capture it. Without sources the output is
/// returned unchanged.
pub fn merge_sources_into_output(output: Option<Value>, sources: &[RunSource]) -> Option<Value> {
    if sources.is_empty() {
        return output;
    }
    let list: Vec<Value> = sources
        .iter()
        .map(|s| {
            let mut entry = serde_json::Map::new();
            entry.insert("id".into(), json!(s.id()));
            entry.insert("title".into(), json!(s.title));
            entry.insert("url".into(), json!(s.url));
            if !s.published.is_empty() {
                entry.insert("published".into(), json!(s.published));
            }
            Value::Object(entry)
        })
        .collect();
    match output {
        Some(Value::Object(mut map)) => {
            // The ids are the host's. A broker-sent `sources` key moves aside.
            if let Some(theirs) = map.remove("sources") {
                map.insert("broker_sources".into(), theirs);
            }
            map.insert("sources".into(), Value::Array(list));
            Some(Value::Object(map))
        }
        Some(other) => Some(json!({ "result": other, "sources": list })),
        None => Some(json!({ "sources": list })),
    }
}

/// Source ids cited in `text` as `[S<n>]` or `[S1, S3]` (`,` or `;` between
/// ids), de-duplicated, in order of first citation. Anything else in brackets
/// is ignored.
pub fn cited_ids(text: &str) -> Vec<i64> {
    let bytes = text.as_bytes();
    let mut out: Vec<i64> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        let Some(close) = text[i + 1..].find(']') else {
            break;
        };
        let inner = &text[i + 1..i + 1 + close];
        if inner.len() <= 64 {
            if let Some(ids) = parse_marker(inner) {
                for id in ids {
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
            }
        }
        i += 1;
    }
    out
}

fn parse_marker(inner: &str) -> Option<Vec<i64>> {
    let mut ids = Vec::new();
    for part in inner.split([',', ';']) {
        let part = part.trim();
        let digits = part.strip_prefix('S')?;
        if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        ids.push(digits.parse().ok()?);
    }
    (!ids.is_empty()).then_some(ids)
}

/// The finish check's result: cited sources in order of first citation, and
/// the cited ids no source answers to.
#[derive(Debug, Clone, PartialEq)]
pub struct Citations {
    pub sources: Vec<Value>,
    pub unresolved: Vec<String>,
}

/// Resolve the markers in `answer` against the run's sources. `None` when the
/// answer has no markers. A source carries no snippet: [`RunSource`] has none.
pub fn resolve_citations(answer: &str, run_sources: &[RunSource]) -> Option<Citations> {
    let ids = cited_ids(answer);
    if ids.is_empty() {
        return None;
    }
    let mut sources = Vec::new();
    let mut unresolved = Vec::new();
    for n in ids {
        match run_sources.iter().find(|s| s.n == n) {
            Some(s) => {
                let mut entry = serde_json::Map::new();
                entry.insert("id".into(), json!(s.id()));
                entry.insert("url".into(), json!(s.url));
                entry.insert("title".into(), json!(s.title));
                if !s.published.is_empty() {
                    entry.insert("published".into(), json!(s.published));
                }
                entry.insert("retrievedAt".into(), json!(s.retrieved_at));
                entry.insert("kind".into(), json!(s.kind.as_str()));
                if !s.artifact_relpath.is_empty() {
                    entry.insert("artifactPath".into(), json!(s.artifact_relpath));
                }
                sources.push(Value::Object(entry));
            }
            None => unresolved.push(format!("S{n}")),
        }
    }
    Some(Citations {
        sources,
        unresolved,
    })
}

/// Add `sources` and `citations.unresolved` to a result object. Keys appear
/// only when they have content.
pub fn apply_citations(result: &mut Value, citations: &Citations) {
    if !citations.sources.is_empty() {
        result["sources"] = Value::Array(citations.sources.clone());
    }
    if !citations.unresolved.is_empty() {
        result["citations"] = json!({ "unresolved": citations.unresolved });
    }
}

/// Record the URLs of a tool result the guest is about to receive: its text
/// and its delivered files. Errors and empty runs record nothing.
pub fn record_response_context(
    store: &SqliteWorkflowStore,
    run_id: &str,
    response: &vam_spec::v1::ExecuteHostToolResponse,
) {
    if run_id.is_empty()
        || response.error.is_some()
        || response.text_result.starts_with("tool_error:")
    {
        return;
    }
    let mut urls = extract_urls(&response.text_result, MAX_CONTEXT_URLS_PER_RESULT);
    let files = std::iter::once(&response.artifact_bytes)
        .chain(response.extra_artifacts.iter().map(|f| &f.bytes))
        .filter(|bytes| !bytes.is_empty() && !bytes.contains(&0));
    for bytes in files {
        if urls.len() >= MAX_CONTEXT_URLS_PER_RESULT {
            break;
        }
        let text = String::from_utf8_lossy(bytes);
        for url in extract_urls(&text, MAX_CONTEXT_URLS_PER_RESULT) {
            if urls.len() < MAX_CONTEXT_URLS_PER_RESULT && !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    if urls.is_empty() {
        return;
    }
    if let Err(err) = store.add_context_urls(run_id, &urls, ContextOrigin::ToolResult) {
        tracing::warn!("context set write failed for {run_id}: {err}");
    }
}

impl SqliteWorkflowStore {
    pub fn initialize_web_sources_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS run_context_urls (
                run_id TEXT NOT NULL,
                url_norm TEXT NOT NULL,
                origin TEXT NOT NULL,
                added_at INTEGER NOT NULL,
                PRIMARY KEY (run_id, url_norm)
            );

            CREATE TABLE IF NOT EXISTS run_sources (
                run_id TEXT NOT NULL,
                n INTEGER NOT NULL,
                url TEXT NOT NULL,
                url_norm TEXT NOT NULL,
                title TEXT NOT NULL,
                published TEXT NOT NULL,
                retrieved_at INTEGER NOT NULL,
                kind TEXT NOT NULL,
                artifact_relpath TEXT NOT NULL,
                PRIMARY KEY (run_id, n),
                UNIQUE (run_id, url_norm)
            );
            ",
        )?;
        Ok(())
    }

    /// Add URLs to a task's context set (raw URLs are normalized here).
    /// Returns how many were new. Stops at [`MAX_CONTEXT_URLS_PER_RUN`].
    pub fn add_context_urls<S: AsRef<str>>(
        &self,
        run_id: &str,
        urls: &[S],
        origin: ContextOrigin,
    ) -> Result<usize, OrchestratorError> {
        if run_id.is_empty() || urls.is_empty() {
            return Ok(0);
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let mut held: usize = transaction.query_row(
            "SELECT COUNT(*) FROM run_context_urls WHERE run_id = ?1",
            params![run_id],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let mut added = 0usize;
        for raw in urls {
            if held >= MAX_CONTEXT_URLS_PER_RUN {
                break;
            }
            let Some(norm) = normalize_url(raw.as_ref()) else {
                continue;
            };
            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO run_context_urls (run_id, url_norm, origin, added_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![run_id, norm, origin.as_str(), now_secs()],
            )?;
            held += inserted;
            added += inserted;
        }
        transaction.commit()?;
        Ok(added)
    }

    /// Copies the context URLs of `prior_runs` (host-recorded sets, never text
    /// parsed from the guest's messages) into `run_id`'s set, up to the cap.
    /// Returns how many were new.
    pub fn inherit_context_urls(
        &self,
        run_id: &str,
        prior_runs: &[String],
    ) -> Result<usize, OrchestratorError> {
        let mut added = 0usize;
        for prior in prior_runs.iter().filter(|r| r.as_str() != run_id) {
            let urls: Vec<String> = {
                let connection = self
                    .connection
                    .lock()
                    .map_err(|_| OrchestratorError::StorageUnavailable)?;
                let mut stmt = connection
                    .prepare("SELECT url_norm FROM run_context_urls WHERE run_id = ?1")?;
                let rows = stmt.query_map(params![prior], |row| row.get::<_, String>(0))?;
                rows.flatten().collect()
            };
            added += self.add_context_urls(run_id, &urls, ContextOrigin::PriorRun)?;
        }
        Ok(added)
    }

    /// Add the URLs found in a task's prompt (at run create).
    pub fn record_prompt_context(
        &self,
        run_id: &str,
        prompt: &str,
    ) -> Result<usize, OrchestratorError> {
        let urls = extract_urls(prompt, MAX_CONTEXT_URLS_PER_RESULT);
        self.add_context_urls(run_id, &urls, ContextOrigin::Prompt)
    }

    /// True when `url` (normalized the same way) is in the task's context set.
    pub fn run_context_contains(&self, run_id: &str, url: &str) -> bool {
        let Some(norm) = normalize_url(url) else {
            return false;
        };
        let Ok(connection) = self.connection.lock() else {
            return false;
        };
        connection
            .query_row(
                "SELECT 1 FROM run_context_urls WHERE run_id = ?1 AND url_norm = ?2",
                params![run_id, norm],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn run_context_count(&self, run_id: &str) -> usize {
        let Ok(connection) = self.connection.lock() else {
            return 0;
        };
        connection
            .query_row(
                "SELECT COUNT(*) FROM run_context_urls WHERE run_id = ?1",
                params![run_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as usize
    }

    /// Number the sources of one tool result. A URL already in the list keeps
    /// its id; a repeat fills in what was empty, upgrades a search result to
    /// a page and records the saved file. Returns the numbered sources (in the
    /// order given, those that could not be numbered left out) and how many
    /// are new.
    pub fn assign_run_sources(
        &self,
        run_id: &str,
        inputs: &[SourceInput],
    ) -> Result<(Vec<RunSource>, usize), OrchestratorError> {
        if run_id.is_empty() || inputs.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let now = now_secs();
        let mut numbered: Vec<RunSource> = Vec::new();
        let mut added = 0usize;
        for input in inputs.iter().take(MAX_SOURCES_PER_CALL) {
            let Some(norm) = normalize_url(&input.url) else {
                continue;
            };
            let title = one_line(&input.title, MAX_TITLE_CHARS);
            let published = one_line(&input.published, MAX_PUBLISHED_CHARS);
            let relpath = if input.artifact_relpath.is_empty() {
                String::new()
            } else {
                sanitize_relpath(&input.artifact_relpath)
            };
            let existing: Option<i64> = transaction
                .query_row(
                    "SELECT n FROM run_sources WHERE run_id = ?1 AND url_norm = ?2",
                    params![run_id, norm],
                    |row| row.get(0),
                )
                .optional()?;
            let n = match existing {
                Some(n) => {
                    transaction.execute(
                        "UPDATE run_sources SET
                            title = CASE WHEN title = '' THEN ?3 ELSE title END,
                            published = CASE WHEN published = '' THEN ?4 ELSE published END,
                            kind = CASE WHEN ?5 = 'page' THEN 'page' ELSE kind END,
                            artifact_relpath =
                                CASE WHEN ?6 != '' THEN ?6 ELSE artifact_relpath END
                         WHERE run_id = ?1 AND n = ?2",
                        params![run_id, n, title, published, input.kind.as_str(), relpath],
                    )?;
                    n
                }
                None => {
                    let (count, next): (i64, i64) = transaction.query_row(
                        "SELECT COUNT(*), COALESCE(MAX(n), 0) + 1
                         FROM run_sources WHERE run_id = ?1",
                        params![run_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                    if count as usize >= MAX_SOURCES_PER_RUN {
                        continue;
                    }
                    transaction.execute(
                        "INSERT INTO run_sources
                            (run_id, n, url, url_norm, title, published, retrieved_at,
                             kind, artifact_relpath)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            run_id,
                            next,
                            norm,
                            norm,
                            title,
                            published,
                            now,
                            input.kind.as_str(),
                            relpath
                        ],
                    )?;
                    added += 1;
                    next
                }
            };
            if let Some(source) = load_source(&transaction, run_id, n)? {
                numbered.push(source);
            }
        }
        transaction.commit()?;
        Ok((numbered, added))
    }

    /// The task's numbered sources, in id order.
    pub fn list_run_sources(&self, run_id: &str) -> Result<Vec<RunSource>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(
            "SELECT n, url, title, published, retrieved_at, kind, artifact_relpath
             FROM run_sources WHERE run_id = ?1 ORDER BY n",
        )?;
        let rows = statement.query_map(params![run_id], row_to_source)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn load_source(
    connection: &rusqlite::Connection,
    run_id: &str,
    n: i64,
) -> Result<Option<RunSource>, OrchestratorError> {
    Ok(connection
        .query_row(
            "SELECT n, url, title, published, retrieved_at, kind, artifact_relpath
             FROM run_sources WHERE run_id = ?1 AND n = ?2",
            params![run_id, n],
            row_to_source,
        )
        .optional()?)
}

fn row_to_source(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunSource> {
    Ok(RunSource {
        n: row.get(0)?,
        url: row.get(1)?,
        title: row.get(2)?,
        published: row.get(3)?,
        retrieved_at: row.get(4)?,
        kind: SourceKind::parse(&row.get::<_, String>(5)?),
        artifact_relpath: row.get(6)?,
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(n: i64, url: &str) -> RunSource {
        RunSource {
            n,
            url: url.to_string(),
            title: format!("T{n}"),
            published: String::new(),
            retrieved_at: 7,
            kind: SourceKind::Page,
            artifact_relpath: String::new(),
        }
    }

    #[test]
    fn cited_ids_parse_single_lists_and_runs_in_first_citation_order() {
        assert_eq!(
            cited_ids("a [S3] b [S1, S3] c [S2;S9][S1]"),
            vec![3, 1, 2, 9]
        );
        assert_eq!(
            cited_ids("[S 1] [s1] [S] [Sx] [S1 S2] [link](http://x) [1]"),
            Vec::<i64>::new()
        );
        assert_eq!(cited_ids("[ S4 , S5 ]"), vec![4, 5]);
        assert!(cited_ids("unclosed [S1").is_empty());
    }

    #[test]
    fn citations_resolve_in_order_and_unknown_ids_are_unresolved() {
        let sources = vec![
            rs(1, "https://a.example/"),
            rs(2, "https://b.example/"),
            rs(3, "https://c.example/"),
        ];
        let c = resolve_citations("x [S3] y [S1] z [S9]", &sources).unwrap();
        let ids: Vec<_> = c
            .sources
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["S3", "S1"]);
        assert_eq!(c.unresolved, ["S9"]);
        assert!(c.sources.iter().all(|s| s.get("snippet").is_none()));
        assert_eq!(c.sources[0]["retrievedAt"], 7);
        assert_eq!(c.sources[0]["kind"], "page");
        assert!(resolve_citations("no markers", &sources).is_none());
        let none = resolve_citations("[S1]", &[]).unwrap();
        assert!(none.sources.is_empty());
        assert_eq!(none.unresolved, ["S1"]);
        let mut result = json!({"answer": "a"});
        apply_citations(&mut result, &none);
        assert!(result.get("sources").is_none());
        assert_eq!(result["citations"]["unresolved"][0], "S1");
    }

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-web";

    fn store() -> SqliteWorkflowStore {
        SqliteWorkflowStore::in_memory().expect("store")
    }

    fn input(url: &str, title: &str, kind: SourceKind) -> SourceInput {
        SourceInput {
            url: url.to_string(),
            title: title.to_string(),
            published: String::new(),
            kind,
            artifact_relpath: String::new(),
        }
    }

    #[test]
    fn normalize_lowercases_and_drops_port_and_fragment() {
        assert_eq!(
            normalize_url("HTTPS://Example.COM:443/A/b?Q=1#frag").as_deref(),
            Some("https://example.com/A/b?Q=1")
        );
        assert_eq!(
            normalize_url("http://example.com:80").as_deref(),
            Some("http://example.com/")
        );
        assert_eq!(
            normalize_url("http://example.com:8080/x").as_deref(),
            Some("http://example.com:8080/x")
        );
    }

    #[test]
    fn normalize_refuses_other_schemes_userinfo_and_long_urls() {
        assert!(normalize_url("javascript:alert(1)").is_none());
        assert!(normalize_url("ftp://example.com/x").is_none());
        assert!(normalize_url("https://user:pw@example.com/").is_none());
        assert!(normalize_url("https://user@example.com/").is_none());
        assert!(normalize_url("not a url").is_none());
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
        assert!(normalize_url(&long).is_none());
    }

    #[test]
    fn a_constructed_url_is_a_different_url() {
        assert_ne!(
            normalize_url("https://example.com/a"),
            normalize_url("https://example.com/a?secret=1")
        );
    }

    #[test]
    fn extract_handles_markdown_json_and_punctuation() {
        let text = "See [docs](https://a.example/x_(y)) and (https://b.example/p). \
                    {\"url\":\"https://c.example/q?a=1&b=2\",\"n\":1} <https://d.example/z>, \
                    https://a.example/x_(y)";
        assert_eq!(
            extract_urls(text, 50),
            vec![
                "https://a.example/x_(y)".to_string(),
                "https://b.example/p".to_string(),
                "https://c.example/q?a=1&b=2".to_string(),
                "https://d.example/z".to_string(),
            ]
        );
    }

    #[test]
    fn extract_respects_the_limit_and_skips_junk() {
        let text = "httpx://no https://ok.example/1 http:// https://ok.example/2";
        assert_eq!(extract_urls(text, 1), vec!["https://ok.example/1"]);
        assert_eq!(extract_urls(text, 10).len(), 2);
    }

    #[test]
    fn context_set_holds_prompt_urls_and_dedupes() {
        let store = store();
        let n = store
            .record_prompt_context(
                RUN,
                "Summarise https://Example.com/a#x and https://example.com/a please",
            )
            .unwrap();
        assert_eq!(n, 1);
        assert!(store.run_context_contains(RUN, "https://example.com/a"));
        assert!(store.run_context_contains(RUN, "HTTPS://EXAMPLE.COM:443/a#other"));
        assert!(!store.run_context_contains(RUN, "https://example.com/a?x=1"));
        assert!(!store.run_context_contains("urn:vam:run:other", "https://example.com/a"));
    }

    #[test]
    fn a_follow_up_inherits_only_the_urls_of_the_runs_in_its_context() {
        let store = store();
        let first = "urn:vam:run:local.vam.dev/acme/run-first";
        let dropped = "urn:vam:run:local.vam.dev/acme/run-dropped";
        store
            .add_context_urls(
                first,
                &["https://seen.example/a"],
                ContextOrigin::ToolResult,
            )
            .unwrap();
        store
            .add_context_urls(
                dropped,
                &["https://gone.example/b"],
                ContextOrigin::ToolResult,
            )
            .unwrap();
        // The new run's context holds only the first run's records.
        let added = store
            .inherit_context_urls(RUN, &[first.to_string(), RUN.to_string()])
            .unwrap();
        assert_eq!(added, 1);
        assert!(store.run_context_contains(RUN, "https://seen.example/a"));
        assert!(!store.run_context_contains(RUN, "https://gone.example/b"));
        // Idempotent.
        assert_eq!(
            store
                .inherit_context_urls(RUN, &[first.to_string()])
                .unwrap(),
            0
        );
    }

    #[test]
    fn context_set_cap_holds_and_keeps_earlier_urls() {
        let store = store();
        let first: Vec<String> = (0..MAX_CONTEXT_URLS_PER_RUN)
            .map(|i| format!("https://cap.example/{i}"))
            .collect();
        assert_eq!(
            store
                .add_context_urls(RUN, &first, ContextOrigin::ToolResult)
                .unwrap(),
            MAX_CONTEXT_URLS_PER_RUN
        );
        let more = vec!["https://cap.example/late".to_string()];
        assert_eq!(
            store
                .add_context_urls(RUN, &more, ContextOrigin::ToolResult)
                .unwrap(),
            0
        );
        assert_eq!(store.run_context_count(RUN), MAX_CONTEXT_URLS_PER_RUN);
        assert!(store.run_context_contains(RUN, "https://cap.example/0"));
        assert!(!store.run_context_contains(RUN, "https://cap.example/late"));
    }

    #[test]
    fn same_url_keeps_its_id_and_a_page_upgrades_a_hit() {
        let store = store();
        let (a, added) = store
            .assign_run_sources(
                RUN,
                &[
                    input("https://a.example/x", "A hit", SourceKind::SearchResult),
                    input("https://b.example/y", "B hit", SourceKind::SearchResult),
                ],
            )
            .unwrap();
        assert_eq!(added, 2);
        assert_eq!(a[0].id(), "S1");
        assert_eq!(a[1].id(), "S2");
        let (b, added) = store
            .assign_run_sources(
                RUN,
                &[
                    input(
                        "https://B.example/y#frag",
                        "other",
                        SourceKind::SearchResult,
                    ),
                    input("https://c.example/z", "C hit", SourceKind::SearchResult),
                    SourceInput {
                        artifact_relpath: "dir/web-a-12345678.md".to_string(),
                        ..input("https://a.example/x", "", SourceKind::Page)
                    },
                ],
            )
            .unwrap();
        assert_eq!(added, 1);
        assert_eq!(b[0].id(), "S2");
        assert_eq!(b[1].id(), "S3");
        assert_eq!(b[2].id(), "S1");
        assert_eq!(b[2].kind, SourceKind::Page);
        assert_eq!(b[2].title, "A hit", "an empty title never replaces one");
        assert_eq!(b[2].artifact_relpath, "web-a-12345678.md");
        assert_eq!(store.list_run_sources(RUN).unwrap().len(), 3);
        assert!(store
            .list_run_sources("urn:vam:run:other")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ids_continue_after_a_restart_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vamd.db");
        {
            let store = SqliteWorkflowStore::open(&path).unwrap();
            store
                .assign_run_sources(RUN, &[input("https://a.example/", "A", SourceKind::Page)])
                .unwrap();
            store
                .record_prompt_context(RUN, "https://p.example/")
                .unwrap();
        }
        let store = SqliteWorkflowStore::open(&path).unwrap();
        let (next, _) = store
            .assign_run_sources(
                RUN,
                &[
                    input("https://b.example/", "B", SourceKind::SearchResult),
                    input("https://a.example/", "A", SourceKind::Page),
                ],
            )
            .unwrap();
        assert_eq!(next[0].id(), "S2");
        assert_eq!(next[1].id(), "S1");
        assert!(store.run_context_contains(RUN, "https://p.example/"));
    }

    #[test]
    fn sources_cap_holds_and_bad_urls_get_no_id() {
        let store = store();
        let many: Vec<SourceInput> = (0..MAX_SOURCES_PER_CALL + 5)
            .map(|i| {
                input(
                    &format!("https://m.example/{i}"),
                    "t",
                    SourceKind::SearchResult,
                )
            })
            .collect();
        let (numbered, added) = store.assign_run_sources(RUN, &many).unwrap();
        assert_eq!(numbered.len(), MAX_SOURCES_PER_CALL);
        assert_eq!(added, MAX_SOURCES_PER_CALL);
        let (numbered, added) = store
            .assign_run_sources(
                RUN,
                &[
                    input("javascript:alert(1)", "x", SourceKind::Page),
                    input("https://u:p@m.example/", "x", SourceKind::Page),
                ],
            )
            .unwrap();
        assert!(numbered.is_empty());
        assert_eq!(added, 0);
        let mut at_cap = 0;
        loop {
            let batch: Vec<SourceInput> = (0..MAX_SOURCES_PER_CALL)
                .map(|i| {
                    input(
                        &format!("https://fill.example/{at_cap}/{i}"),
                        "t",
                        SourceKind::SearchResult,
                    )
                })
                .collect();
            let (_, added) = store.assign_run_sources(RUN, &batch).unwrap();
            at_cap += 1;
            if added == 0 || at_cap > 40 {
                break;
            }
        }
        assert_eq!(
            store.list_run_sources(RUN).unwrap().len(),
            MAX_SOURCES_PER_RUN
        );
    }

    #[test]
    fn title_is_one_line_and_capped() {
        let store = store();
        let (numbered, _) = store
            .assign_run_sources(
                RUN,
                &[input(
                    "https://t.example/",
                    &format!("line one\n[S99] forged\t{}", "x".repeat(500)),
                    SourceKind::Page,
                )],
            )
            .unwrap();
        assert!(!numbered[0].title.contains('\n'));
        assert!(numbered[0].title.chars().count() <= MAX_TITLE_CHARS);
    }

    #[test]
    fn schema_has_no_snippet_or_content_column() {
        let store = store();
        let connection = store.connection.lock().unwrap();
        for table in ["run_sources", "run_context_urls"] {
            let mut statement = connection
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let columns: Vec<String> = statement
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .map(|c| c.unwrap())
                .collect();
            for column in &columns {
                for banned in ["snippet", "content", "text", "body", "excerpt"] {
                    assert!(!column.contains(banned), "{table}.{column}");
                }
            }
        }
    }

    #[test]
    fn merge_adds_sources_to_an_object_and_keeps_it_json() {
        let source = RunSource {
            n: 1,
            url: "https://a.example/".into(),
            title: "A".into(),
            published: "2026-01-02".into(),
            retrieved_at: 0,
            kind: SourceKind::SearchResult,
            artifact_relpath: String::new(),
        };
        let merged = merge_sources_into_output(
            Some(json!({"provider": "mock", "sources": "theirs"})),
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert_eq!(merged["sources"][0]["id"], "S1");
        assert_eq!(merged["sources"][0]["published"], "2026-01-02");
        assert_eq!(merged["broker_sources"], "theirs");
        assert_eq!(merged["provider"], "mock");
        let wrapped =
            merge_sources_into_output(Some(json!([1, 2])), std::slice::from_ref(&source)).unwrap();
        assert_eq!(wrapped["result"], json!([1, 2]));
        assert_eq!(wrapped["sources"][0]["url"], "https://a.example/");
        let bare = merge_sources_into_output(None, std::slice::from_ref(&source)).unwrap();
        assert_eq!(bare["sources"][0]["id"], "S1");
        assert_eq!(
            merge_sources_into_output(Some(json!({"a": 1})), &[]),
            Some(json!({"a": 1}))
        );
    }

    #[test]
    fn relpath_is_flat_and_safe() {
        assert_eq!(sanitize_relpath("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_relpath(".hidden"), "hidden");
        assert_eq!(sanitize_relpath("a b/c d.md"), "c_d.md");
        assert_eq!(sanitize_relpath(""), "artifact.bin");
        assert!(sanitize_relpath(&"x".repeat(500)).len() <= MAX_RELPATH_CHARS);
    }

    #[test]
    fn response_context_records_text_and_files_but_not_errors() {
        let store = store();
        let response = vam_spec::v1::ExecuteHostToolResponse {
            text_result: "see https://t.example/one".to_string(),
            artifact_relpath: "p.md".to_string(),
            artifact_bytes: b"link [x](https://t.example/two)".to_vec(),
            extra_artifacts: vec![vam_spec::v1::HostArtifactFile {
                relpath: "q.md".into(),
                bytes: b"https://t.example/three".to_vec(),
            }],
            error: None,
        };
        record_response_context(&store, RUN, &response);
        for n in ["one", "two", "three"] {
            assert!(store.run_context_contains(RUN, &format!("https://t.example/{n}")));
        }
        let failed = vam_spec::v1::ExecuteHostToolResponse {
            text_result: "tool_error: see https://bad.example/x".to_string(),
            ..Default::default()
        };
        record_response_context(&store, RUN, &failed);
        assert!(!store.run_context_contains(RUN, "https://bad.example/x"));
        record_response_context(&store, "", &response);
    }
}
