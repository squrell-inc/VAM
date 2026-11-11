//! Pages served from files, for hermetic runs (`--provider mock`). No
//! network, no robots.txt: a fixture is whatever the operator put on disk.
//!
//! For a URL, looks in `<dir>/pages/` for `<key>.html`, `<key>.txt` or
//! `<key>.md` (then `default.*`), where `key` is the first 16 hex characters
//! of the SHA-256 of the trimmed URL. An optional `<key>.json` sidecar can
//! set `content_type` or `final_url`, answer a cross-host `redirect_to`, or
//! force a typed `error` (`blocked`, `blocked_by_robots`, `too_large`,
//! `timeout`, `unsupported_content_type`, `rate_limited`, `not_accessible`).
//! The page then goes through the same conversion as a fetched one.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use url::Url;

use super::artifact::rfc3339_utc;
use super::content::mime_essence;
use super::rules::{check_url, parse_request_url};
use super::{
    convert_blocking, FetchOutcome, Page, PageFetcher, MAX_CONCURRENT_CONVERSIONS, MAX_RAW_BYTES,
};
use crate::error::FetchError;

/// Serves pages from a fixture directory.
pub struct FixtureFetcher {
    dir: PathBuf,
    permits: Arc<Semaphore>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sidecar {
    content_type: Option<String>,
    final_url: Option<String>,
    redirect_to: Option<String>,
    error: Option<String>,
}

impl FixtureFetcher {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_CONVERSIONS)),
        }
    }

    /// File stem used for a URL.
    pub fn fixture_key(url: &str) -> String {
        let digest = Sha256::digest(url.trim().as_bytes());
        digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    fn find(&self, stems: &[String]) -> Option<(PathBuf, &'static str)> {
        let pages = self.dir.join("pages");
        for stem in stems {
            for (ext, mime) in [
                ("html", "text/html; charset=utf-8"),
                ("txt", "text/plain; charset=utf-8"),
                ("md", "text/markdown; charset=utf-8"),
            ] {
                let path = pages.join(format!("{stem}.{ext}"));
                if path.is_file() {
                    return Some((path, mime));
                }
            }
        }
        None
    }
}

fn typed(code: &str) -> Option<FetchError> {
    Some(match code {
        "blocked" => FetchError::Blocked("the fixture marks this address as blocked".to_string()),
        "blocked_by_robots" => FetchError::BlockedByRobots,
        "too_large" => FetchError::TooLarge,
        "timeout" => FetchError::Timeout,
        "unsupported_content_type" => {
            FetchError::UnsupportedContentType("of an unknown type".to_string())
        }
        "rate_limited" => FetchError::RateLimited,
        "not_accessible" => {
            FetchError::not_accessible("the fixture marks this page as not accessible")
        }
        _ => return None,
    })
}

#[async_trait]
impl PageFetcher for FixtureFetcher {
    async fn fetch(&self, url: &str, _timeout: Duration) -> Result<FetchOutcome, FetchError> {
        let requested = parse_request_url(url, false)?;
        let key = Self::fixture_key(url);
        let stems = [key.clone(), "default".to_string()];
        let sidecar: Sidecar =
            match std::fs::read(self.dir.join("pages").join(format!("{key}.json"))) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
                    FetchError::not_accessible("the page fixture sidecar is not valid")
                })?,
                Err(_) => Sidecar::default(),
            };
        if let Some(code) = &sidecar.error {
            return Err(typed(code).unwrap_or_else(|| {
                FetchError::not_accessible("the page fixture names an unknown error")
            }));
        }
        if let Some(target) = &sidecar.redirect_to {
            let target = Url::parse(target)
                .ok()
                .filter(|t| check_url(t, false).is_ok())
                .ok_or_else(|| {
                    FetchError::not_accessible("the fixture redirect target is not valid")
                })?;
            return Ok(FetchOutcome::Redirected {
                requested_url: requested.to_string(),
                final_url: requested.to_string(),
                redirect_to: target.to_string(),
            });
        }
        let (path, default_type) = self.find(&stems).ok_or_else(|| {
            FetchError::not_accessible("no page fixture exists for this URL (mock provider)")
        })?;
        let body = std::fs::read(&path)
            .map_err(|_| FetchError::not_accessible("the page fixture could not be read"))?;
        if body.len() > MAX_RAW_BYTES {
            return Err(FetchError::TooLarge);
        }
        let content_type = sidecar
            .content_type
            .unwrap_or_else(|| default_type.to_string());
        let final_url = match &sidecar.final_url {
            Some(f) => Url::parse(f)
                .ok()
                .filter(|u| check_url(u, false).is_ok())
                .ok_or_else(|| FetchError::not_accessible("the fixture final_url is not valid"))?,
            None => requested.clone(),
        };
        let raw_bytes = body.len();
        let converted = convert_blocking(
            &self.permits,
            body,
            Some(content_type.clone()),
            final_url.clone(),
        )
        .await?;
        Ok(FetchOutcome::Page(Box::new(Page {
            requested_url: requested.to_string(),
            final_url,
            title: converted.title,
            markdown: converted.markdown,
            content_type: mime_essence(&content_type),
            raw_bytes,
            retrieved_at: rfc3339_utc(SystemTime::now()),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir.join("pages")).expect("mkdir");
        std::fs::write(dir.join("pages").join(name), body).expect("write");
    }

    #[tokio::test]
    async fn serves_keyed_default_redirect_and_error_fixtures() {
        let dir = tempfile::tempdir().expect("tmp");
        let url = "https://fixture.test/page";
        let key = FixtureFetcher::fixture_key(url);
        write(
            dir.path(),
            &format!("{key}.html"),
            "<html><head><title>Fixture</title></head><body><p>hello fixture</p></body></html>",
        );
        write(dir.path(), "default.txt", "plain default page");
        write(
            dir.path(),
            &format!(
                "{}.json",
                FixtureFetcher::fixture_key("https://moved.test/")
            ),
            r#"{"redirect_to":"https://elsewhere.test/new"}"#,
        );
        write(
            dir.path(),
            &format!("{}.json", FixtureFetcher::fixture_key("https://bad.test/")),
            r#"{"error":"blocked_by_robots"}"#,
        );
        let f = FixtureFetcher::new(dir.path().to_path_buf());
        let t = Duration::from_secs(5);

        let FetchOutcome::Page(p) = f.fetch(url, t).await.expect("page") else {
            panic!("page expected");
        };
        assert_eq!(p.title, "Fixture");
        assert!(p.markdown.contains("hello fixture"));
        assert_eq!(p.content_type, "text/html");

        let FetchOutcome::Page(d) = f.fetch("https://other.test/", t).await.expect("default")
        else {
            panic!("page expected");
        };
        assert_eq!(d.markdown, "plain default page");

        match f.fetch("https://moved.test/", t).await.expect("redirect") {
            FetchOutcome::Redirected { redirect_to, .. } => {
                assert_eq!(redirect_to, "https://elsewhere.test/new")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            f.fetch("https://bad.test/", t).await.expect_err("robots"),
            FetchError::BlockedByRobots
        );
        // URL rules still apply in fixture mode.
        assert!(matches!(
            f.fetch("http://localhost:8080/", t).await,
            Err(FetchError::InvalidUrl(_))
        ));
        let empty = tempfile::tempdir().expect("tmp");
        let none = FixtureFetcher::new(empty.path().to_path_buf());
        assert_eq!(
            none.fetch("https://x.test/", t)
                .await
                .expect_err("missing")
                .code(),
            "not_accessible"
        );
    }
}
