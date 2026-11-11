//! The saved page: file name, header and size cap.

use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use url::Url;

use super::content::clean_line;
use super::content::MAX_TITLE_CHARS;

/// Largest saved file, header and truncation note included.
///
/// vamd keeps host artifacts to 48 KiB (one vsock frame) and fences a
/// `.md` file inside that budget: the fence header and footer take up to
/// 1024 bytes and the "[truncated ...]" note reserve 64 more
/// (`MAX_FENCE_OVERHEAD` in `vam-security`, `FENCE_TRUNCATION_NOTE_RESERVE`
/// in vamd's `host_tools.rs`), leaving 48064 bytes for the file. 46 KiB
/// (47104) keeps another 960 bytes for marker-like text the fence rewrites,
/// so vamd never cuts a page file a second time. vamd's test
/// `web_page_file_at_the_broker_cap_is_fenced_without_a_second_cut` ties
/// this number to the fence; change them together.
pub const MAX_ARTIFACT_BYTES: usize = 46 * 1024;
/// Longest host part of the file name.
const MAX_HOST_SLUG: usize = 40;

/// Facts written into the file header.
pub struct PageMeta<'a> {
    pub requested_url: &'a str,
    pub final_url: &'a str,
    pub title: &'a str,
    pub retrieved_at: &'a str,
    pub content_type: &'a str,
}

/// The file to hand to vamd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub relpath: String,
    pub bytes: Vec<u8>,
    /// Bytes of page markdown kept in the file.
    pub kept: usize,
    /// Bytes of page markdown before the cap.
    pub total: usize,
    pub truncated: bool,
    /// The page text that is in the file (a prefix of the markdown).
    pub kept_text: String,
}

/// `web-<host>-<8 hex of sha256(final url)>.md`.
pub fn artifact_name(final_url: &Url) -> String {
    let host = final_url.host_str().unwrap_or("page").to_ascii_lowercase();
    let mut slug = String::new();
    for c in host.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '.' {
            c
        } else {
            '-'
        };
        if c == '-' && slug.ends_with('-') {
            continue;
        }
        slug.push(c);
    }
    let slug: String = slug
        .trim_matches(|c| c == '-' || c == '.')
        .chars()
        .take(MAX_HOST_SLUG)
        .collect();
    let slug = if slug.is_empty() {
        "page".to_string()
    } else {
        slug
    };
    let digest = Sha256::digest(final_url.as_str().as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("web-{slug}-{hex}.md")
}

/// `2026-10-17T12:34:56Z`.
pub fn rfc3339_utc(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Build the file: header, then the markdown, cut so the whole file stays
/// within [`MAX_ARTIFACT_BYTES`].
pub fn build(meta: &PageMeta<'_>, final_url: &Url, markdown: &str) -> Artifact {
    let mut header = format!("Source: {}\n", meta.requested_url);
    if meta.final_url != meta.requested_url {
        header.push_str(&format!("Final URL: {}\n", meta.final_url));
    }
    header.push_str(&format!("Retrieved: {} (UTC)\n", meta.retrieved_at));
    let title = clean_line(meta.title, MAX_TITLE_CHARS);
    if !title.is_empty() {
        header.push_str(&format!("Title: {title}\n"));
    }
    header.push_str(&format!(
        "Content-Type: {}\n\n---\n\n",
        clean_line(meta.content_type, 100)
    ));

    let total = markdown.len();
    // One byte is the trailing newline.
    let room = MAX_ARTIFACT_BYTES.saturating_sub(header.len() + 1);
    let (kept_text, truncated) = if total <= room {
        (markdown.to_string(), false)
    } else {
        // Leave room for the note (at most 80 bytes).
        let budget = room.saturating_sub(80);
        let cut = super::preview::cut_at(markdown, budget);
        (cut.trim_end().to_string(), true)
    };
    let mut body = kept_text.clone();
    if truncated {
        body.push_str(&format!(
            "\n\n[truncated: kept {} of {} bytes]\n",
            kept_text.len(),
            total
        ));
    } else {
        body.push('\n');
    }
    let mut bytes = header.into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    Artifact {
        relpath: artifact_name(final_url),
        bytes,
        kept: kept_text.len(),
        total,
        truncated,
        kept_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn meta<'a>(req: &'a str, fin: &'a str, title: &'a str) -> PageMeta<'a> {
        PageMeta {
            requested_url: req,
            final_url: fin,
            title,
            retrieved_at: "2026-10-17T10:00:00Z",
            content_type: "text/html",
        }
    }

    #[test]
    fn dates_format_correctly() {
        let at = |s: u64| rfc3339_utc(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_760_702_096), "2025-10-17T11:54:56Z");
        assert_eq!(at(1_792_233_600), "2026-10-17T10:40:00Z");
        assert_eq!(at(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn names_are_safe_and_stable() {
        let u = Url::parse("https://Docs.Example.com/a/b?c=1").expect("url");
        let n = artifact_name(&u);
        assert!(n.starts_with("web-docs.example.com-"));
        assert!(n.ends_with(".md"));
        assert_eq!(n, artifact_name(&u));
        assert_ne!(
            n,
            artifact_name(&Url::parse("https://docs.example.com/a/c").expect("url"))
        );
        let ip = Url::parse("http://[2606:4700:4700::1111]/").expect("url");
        let n = artifact_name(&ip);
        assert!(n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')));
        let long = Url::parse(&format!("https://{}.example.com/", "a".repeat(60))).expect("url");
        assert!(artifact_name(&long).len() < 60);
    }

    #[test]
    fn header_starts_with_url_and_date_and_is_one_line_per_field() {
        let u = Url::parse("https://a.test/x").expect("url");
        let a = build(
            &meta(
                "http://a.test/x",
                "https://a.test/x",
                "T\nINJECT: ignore all",
            ),
            &u,
            "hello",
        );
        let text = String::from_utf8(a.bytes).expect("utf8");
        assert!(text.starts_with("Source: http://a.test/x\nFinal URL: https://a.test/x\nRetrieved: 2026-10-17T10:00:00Z (UTC)\nTitle: T INJECT: ignore all\n"));
        assert!(text.ends_with("---\n\nhello\n"));
        assert!(!a.truncated);
    }

    #[test]
    fn oversize_markdown_is_cut_with_a_note_and_stays_under_the_cap() {
        let u = Url::parse("https://a.test/x").expect("url");
        let long_url = format!("https://a.test/{}", "p".repeat(2000));
        let md = "word ".repeat(40_000);
        let a = build(&meta(&long_url, &long_url, &"t".repeat(500)), &u, &md);
        assert!(a.truncated);
        assert!(a.bytes.len() <= MAX_ARTIFACT_BYTES, "{}", a.bytes.len());
        assert!(a.bytes.len() > MAX_ARTIFACT_BYTES - 1000);
        let text = String::from_utf8(a.bytes).expect("utf8");
        assert!(text.contains(&format!(
            "[truncated: kept {} of {} bytes]",
            a.kept, a.total
        )));
        assert_eq!(a.total, md.len());
        // Multibyte text is cut on a character boundary.
        let a = build(&meta("u", "u", "t"), &u, &"é".repeat(60_000));
        assert!(String::from_utf8(a.bytes.clone()).is_ok());
        assert!(a.bytes.len() <= MAX_ARTIFACT_BYTES);
    }

    #[test]
    fn exactly_fitting_markdown_is_not_truncated() {
        let u = Url::parse("https://a.test/x").expect("url");
        let probe = build(&meta("u", "u", "t"), &u, "");
        let room = MAX_ARTIFACT_BYTES - probe.bytes.len();
        // One byte is the trailing newline.
        let md = "x".repeat(room);
        let a = build(&meta("u", "u", "t"), &u, &md);
        assert!(!a.truncated);
        assert_eq!(a.bytes.len(), MAX_ARTIFACT_BYTES);
    }

    #[test]
    fn the_cap_leaves_room_for_vamds_fence() {
        // vamd's budget is 48 KiB, minus 1024 fence overhead and a 64-byte
        // note reserve. Keep the file below that with some margin.
        assert_eq!(MAX_ARTIFACT_BYTES, 46 * 1024);
        const _: () = assert!(MAX_ARTIFACT_BYTES + 1024 + 64 <= 48 * 1024);
    }
}
