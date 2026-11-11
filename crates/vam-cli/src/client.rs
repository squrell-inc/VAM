use anyhow::{Context, Result};
use prost::Message;
use uuid::Uuid;

use crate::config::ResolvedConfig;

/// Declares this client's surface on every request, so a decision made from
/// the CLI is recorded as one.
fn surface_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::HeaderName::from_static("x-vam-surface"),
        reqwest::header::HeaderValue::from_static("cli"),
    );
    headers
}

pub struct Client {
    base: String,
    api_key: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(cfg: &ResolvedConfig) -> Result<Self> {
        crate::transport::warn_cleartext(&cfg.host, "API key");
        Ok(Self {
            base: cfg.host.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            http: crate::transport::client_builder()?
                .default_headers(surface_headers())
                .build()
                .context("build HTTP client")?,
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub async fn get(&self, path: &str) -> Result<String> {
        let resp = self
            .http
            .get(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .context("GET request failed")?;
        let status = resp.status();
        let body = resp.text().await.context("read response body")?;
        if !status.is_success() {
            return Err(http_reject("GET", path, status, body));
        }
        Ok(body)
    }

    /// GET that returns raw response bytes (e.g. `application/octet-stream`).
    /// On non-success, reads the error body as text and maps via [`http_reject`].
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .context("GET request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.context("read error response body")?;
            return Err(http_reject("GET", path, status, body));
        }
        let bytes = resp.bytes().await.context("read response body")?;
        Ok(bytes.to_vec())
    }

    /// PUT raw bytes (e.g. workspace file upload as `application/octet-stream`).
    pub async fn put_bytes(&self, path: &str, bytes: &[u8]) -> Result<String> {
        let resp = self
            .http
            .put(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .await
            .context("PUT request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("read response body")?;
        if !status.is_success() {
            return Err(http_reject("PUT", path, status, text));
        }
        Ok(text)
    }

    pub async fn get_mutating(&self, path: &str, idempotency_key: &str) -> Result<String> {
        let resp = self
            .http
            .get(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Idempotency-Key", idempotency_key)
            .send()
            .await
            .context("GET request failed")?;
        let status = resp.status();
        let body = resp.text().await.context("read response body")?;
        if !status.is_success() {
            return Err(http_reject("GET", path, status, body));
        }
        Ok(body)
    }

    pub async fn delete(&self, path: &str, idempotency_key: Option<&str>) -> Result<String> {
        let idem = header_key(idempotency_key);
        let resp = self
            .http
            .delete(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Idempotency-Key", &idem)
            .send()
            .await
            .context("DELETE request failed")?;
        let status = resp.status();
        let body = resp.text().await.context("read response body")?;
        if !status.is_success() {
            return Err(http_reject("DELETE", path, status, body));
        }
        Ok(body)
    }

    pub async fn put_json(
        &self,
        path: &str,
        proto_name: &str,
        msg: &impl Message,
        idempotency_key: Option<&str>,
    ) -> Result<String> {
        let idem = header_key(idempotency_key);
        let body =
            vam_spec::encoding::proto_to_json(proto_name, msg).map_err(|e| anyhow::anyhow!(e))?;
        let resp = self
            .http
            .put(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Idempotency-Key", &idem)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .context("PUT request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("read response body")?;
        if !status.is_success() {
            return Err(http_reject("PUT", path, status, text));
        }
        Ok(text)
    }

    pub async fn put_raw_json(
        &self,
        path: &str,
        body: &str,
        idempotency_key: Option<&str>,
    ) -> Result<String> {
        let idem = header_key(idempotency_key);
        let resp = self
            .http
            .put(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Idempotency-Key", &idem)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .context("PUT request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("read response body")?;
        if !status.is_success() {
            return Err(http_reject("PUT", path, status, text));
        }
        Ok(text)
    }

    pub async fn patch_raw_json(
        &self,
        path: &str,
        body: &str,
        idempotency_key: Option<&str>,
    ) -> Result<String> {
        let idem = header_key(idempotency_key);
        let resp = self
            .http
            .patch(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Idempotency-Key", &idem)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .context("PATCH request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("read response body")?;
        if !status.is_success() {
            return Err(http_reject("PATCH", path, status, text));
        }
        Ok(text)
    }

    pub async fn post_json(
        &self,
        path: &str,
        proto_name: &str,
        msg: &impl Message,
        idempotency_key: Option<&str>,
    ) -> Result<String> {
        let idem = header_key(idempotency_key);
        let body =
            vam_spec::encoding::proto_to_json(proto_name, msg).map_err(|e| anyhow::anyhow!(e))?;
        self.post_raw_json(path, &body, &idem).await
    }

    pub async fn post_raw_json(
        &self,
        path: &str,
        body: &str,
        idempotency_key: &str,
    ) -> Result<String> {
        let mut req = self
            .http
            .post(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Idempotency-Key", idempotency_key)
            .header("Content-Type", "application/json");
        if let Some(tp) = std::env::var("VAM_TRACEPARENT")
            .ok()
            .filter(|v| !v.is_empty())
        {
            req = req.header("traceparent", tp);
        }
        let resp = req
            .body(body.to_string())
            .send()
            .await
            .context("POST request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("read response body")?;
        if !status.is_success() && status != reqwest::StatusCode::ACCEPTED {
            return Err(http_reject("POST", path, status, text));
        }
        Ok(text)
    }

    pub async fn post_multipart(
        &self,
        path: &str,
        part_name: &str,
        filename: &str,
        content_type: &str,
        bytes: Vec<u8>,
        idempotency_key: Option<&str>,
    ) -> Result<String> {
        let idem = header_key(idempotency_key);
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_string())
            .mime_str(content_type)
            .context("multipart mime type")?;
        let form = reqwest::multipart::Form::new().part(part_name.to_string(), part);
        let resp = self
            .http
            .post(self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Idempotency-Key", &idem)
            .multipart(form)
            .send()
            .await
            .context("POST multipart request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("read response body")?;
        if !status.is_success() {
            return Err(http_reject("POST", path, status, text));
        }
        Ok(text)
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Send a request and return the status and body, including error responses.
    ///
    /// PUT, POST, and DELETE send `Idempotency-Key` (minted when `idempotency_key`
    /// is empty). Callers decide how to treat a non-success status.
    pub async fn exchange(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> Result<HttpReply> {
        let mut req = self
            .http
            .request(method.clone(), self.url(path))
            .header("Authorization", format!("Bearer {}", self.api_key));
        if method == reqwest::Method::PUT
            || method == reqwest::Method::POST
            || method == reqwest::Method::DELETE
        {
            req = req.header("Idempotency-Key", header_key(idempotency_key));
        }
        if let Some(body) = body {
            req = req
                .header("Content-Type", "application/json")
                .body(body.to_string());
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("{method} {path} failed"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.context("read response body")?;
        Ok(HttpReply { status, body: text })
    }
}

/// Status and body from [`Client::exchange`], success or not.
pub struct HttpReply {
    pub status: u16,
    pub body: String,
}

pub async fn login(
    host: &str,
    username: &str,
    password: &str,
) -> Result<crate::config::SessionCreds> {
    crate::transport::warn_cleartext(host, "password");
    let http = crate::transport::client_builder()?
        .build()
        .context("build HTTP client")?;
    let url = format!("{}/v1/login", host.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "username": username,
            "password": password,
        }))
        .send()
        .await
        .context("POST /v1/login failed")?;
    let status = resp.status();
    let text = resp.text().await.context("read login response")?;
    if !status.is_success() {
        return Err(http_reject("POST", "/v1/login", status, text));
    }
    serde_json::from_str(&text).with_context(|| format!("parse login response: {text}"))
}

pub async fn logout(host: &str, token: &str) -> Result<String> {
    crate::transport::warn_cleartext(host, "session token");
    let http = crate::transport::client_builder()?
        .build()
        .context("build HTTP client")?;
    let url = format!("{}/v1/logout", host.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .context("POST /v1/logout failed")?;
    let status = resp.status();
    let text = resp.text().await.context("read logout response")?;
    if !status.is_success() {
        return Err(http_reject("POST", "/v1/logout", status, text));
    }
    Ok(text)
}

pub async fn refresh_session(
    host: &str,
    refresh_token: &str,
) -> Result<crate::config::SessionCreds> {
    crate::transport::warn_cleartext(host, "refresh token");
    let http = crate::transport::client_builder()?
        .build()
        .context("build HTTP client")?;
    let url = format!("{}/v1/login/refresh", host.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "refreshToken": refresh_token }))
        .send()
        .await
        .context("POST /v1/login/refresh failed")?;
    let status = resp.status();
    let text = resp.text().await.context("read refresh response")?;
    if !status.is_success() {
        return Err(http_reject("POST", "/v1/login/refresh", status, text));
    }
    serde_json::from_str(&text).with_context(|| format!("parse refresh response: {text}"))
}

pub fn new_idempotency_key() -> String {
    Uuid::new_v4().to_string()
}

/// Use the caller's key when it is non-empty. Otherwise mint one.
pub fn resolve_idempotency_key(key: Option<String>) -> String {
    header_key(key.as_deref())
}

fn header_key(key: Option<&str>) -> String {
    match key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(key) => key.to_string(),
        None => new_idempotency_key(),
    }
}

fn http_reject(
    method: &str,
    path: &str,
    status: reqwest::StatusCode,
    body: String,
) -> anyhow::Error {
    crate::http_error::failure(method, path, status.as_u16(), &body)
}

/// Percent-encode a URN or id for use as a single URL path segment.
pub fn encode_path_segment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

/// Decode a percent-encoded path segment back to the canonical id/URN.
pub fn decode_path_segment(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                anyhow::bail!("invalid percent-encoding in resource id");
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .context("invalid percent-encoding in resource id")?;
            let byte =
                u8::from_str_radix(hex, 16).context("invalid percent-encoding in resource id")?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).context("invalid UTF-8 in resource id")
}

/// Accept raw or percent-encoded resource ids from the shell.
pub fn normalize_resource_id(id: &str) -> Result<String> {
    let trimmed = id.trim();
    if trimmed.contains('%') {
        decode_path_segment(trimmed)
    } else {
        Ok(trimmed.to_string())
    }
}

pub fn parse_json<T: Message + Default>(proto_name: &str, json: &str) -> Result<T> {
    vam_spec::encoding::json_to_proto(proto_name, json).map_err(|e| anyhow::anyhow!(e))
}

pub fn print_json(text: &str) {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        if crate::output::is_json() {
            println!("{v}");
            return;
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string())
        );
    } else if crate::output::is_json() {
        println!("{}", serde_json::json!({ "raw": text }));
    } else {
        println!("{}", text);
    }
}

/// The most pages `--all` follows before it stops (50,000 rows at the default size).
const MAX_PAGES: usize = 1000;

/// Merges list pages: the first page with every page's `array_key` rows in order and no
/// `pagination` token left.
pub fn merge_pages(pages: Vec<serde_json::Value>, array_key: &str) -> serde_json::Value {
    let mut rows = Vec::new();
    let mut first = serde_json::Value::Null;
    for (i, mut page) in pages.into_iter().enumerate() {
        if let Some(a) = page.get_mut(array_key).and_then(|v| v.as_array_mut()) {
            rows.append(a);
        }
        if i == 0 {
            first = page;
        }
    }
    if let Some(o) = first.as_object_mut() {
        o.insert(array_key.to_string(), serde_json::Value::Array(rows));
        o.remove("pagination");
    }
    first
}

fn next_token(page: &serde_json::Value) -> Option<String> {
    page.get("pagination")
        .and_then(|p| p.get("nextPageToken"))
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// `GET path` for a paged list (runs, sessions, approvals). Without `all` it returns the first
/// page and says on stderr that more exist; with `all` it follows `nextPageToken` and returns
/// one merged answer. A node that does not page answers one page and that is all there is.
pub async fn list_paged(c: &Client, path: &str, array_key: &str, all: bool) -> Result<String> {
    let first = c.get(path).await?;
    let Ok(page) = serde_json::from_str::<serde_json::Value>(&first) else {
        return Ok(first);
    };
    let Some(mut token) = next_token(&page) else {
        return Ok(first);
    };
    if !all {
        eprintln!(
            "more results exist: this is the first page; pass --all to list every page (next page token {token})"
        );
        return Ok(first);
    }
    let mut pages = vec![page];
    while pages.len() < MAX_PAGES {
        let body = c.get(&format!("{path}?pageToken={token}")).await?;
        let page: serde_json::Value = serde_json::from_str(&body).context("read a list page")?;
        let next = next_token(&page);
        pages.push(page);
        match next {
            Some(t) => token = t,
            None => return Ok(merge_pages(pages, array_key).to_string()),
        }
    }
    eprintln!("stopped after {MAX_PAGES} pages; more results exist");
    Ok(merge_pages(pages, array_key).to_string())
}

#[cfg(test)]
mod paging_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merged_pages_keep_order_and_drop_the_token() {
        let m = merge_pages(
            vec![
                json!({"statuses": [1, 2], "pagination": {"nextPageToken": "t"}}),
                json!({"statuses": [3], "pagination": {"nextPageToken": ""}}),
            ],
            "statuses",
        );
        assert_eq!(m, json!({"statuses": [1, 2, 3]}));
    }

    #[test]
    fn next_token_is_none_when_empty_or_absent() {
        assert_eq!(
            next_token(&json!({"pagination": {"nextPageToken": ""}})),
            None
        );
        assert_eq!(next_token(&json!({})), None);
        assert_eq!(
            next_token(&json!({"pagination": {"nextPageToken": "x"}})).as_deref(),
            Some("x")
        );
    }
}

#[cfg(test)]
mod surface_header_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn every_request_declares_the_cli_surface() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
            String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase()
        });
        let client = Client::new(&ResolvedConfig {
            host: format!("http://{addr}"),
            api_key: "k".into(),
        })
        .unwrap();
        client.get("/v1/x").await.unwrap();
        let request = server.await.unwrap();
        assert!(request.contains("x-vam-surface: cli"), "{request}");
    }
}
