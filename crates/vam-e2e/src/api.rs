//! A blocking HTTP client for vamd: Bearer key, `Idempotency-Key` on writes, JSON bodies.

use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::Duration;

/// A reply from vamd, success or not.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// The transport failed (connection refused, timeout): vamd is not there.
#[derive(Debug, Clone)]
pub struct ConnError(pub String);

pub struct Api {
    base: String,
    key: String,
    tag: String,
    http: RefCell<reqwest::blocking::Client>,
    /// The node's certificate to trust, read the first time it exists (vamd writes it on its
    /// first start, after this client is built).
    ca_path: Option<PathBuf>,
    ca_loaded: Cell<bool>,
    n: Cell<u64>,
}

fn base_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder().timeout(Duration::from_secs(10))
}

impl Api {
    pub fn new(base: &str, key: &str, tag: &str) -> Api {
        Api::with_ca_path(base, key, tag, None)
    }

    /// A client for an `https://` node that trusts only the certificate in `ca_path`.
    /// Until that file exists every call fails as a transport error ("no node certificate yet"),
    /// so a health wait simply keeps polling.
    pub fn with_ca_path(base: &str, key: &str, tag: &str, ca_path: Option<PathBuf>) -> Api {
        Api {
            base: base.trim_end_matches('/').to_string(),
            key: key.to_string(),
            tag: tag.to_string(),
            http: RefCell::new(base_builder().build().expect("http client")),
            ca_path,
            ca_loaded: Cell::new(false),
            n: Cell::new(0),
        }
    }

    /// The HTTP client, rebuilt once with the node's certificate when that file first exists.
    fn client(&self) -> Result<reqwest::blocking::Client, ConnError> {
        if let Some(path) = &self.ca_path {
            if !self.ca_loaded.get() {
                let pem = std::fs::read(path).map_err(|e| {
                    ConnError(format!(
                        "no node certificate at {} yet: {e}",
                        path.display()
                    ))
                })?;
                // reqwest accepts any bytes here and only trusts nothing later: check first.
                if !String::from_utf8_lossy(&pem).contains("-----BEGIN CERTIFICATE-----") {
                    return Err(ConnError(format!(
                        "unreadable node certificate {}: no PEM certificate in it",
                        path.display()
                    )));
                }
                let cert = reqwest::Certificate::from_pem(&pem).map_err(|e| {
                    ConnError(format!(
                        "unreadable node certificate {}: {e}",
                        path.display()
                    ))
                })?;
                let client = base_builder()
                    .tls_built_in_root_certs(false)
                    .add_root_certificate(cert)
                    .build()
                    .map_err(|e| ConnError(format!("http client: {e}")))?;
                *self.http.borrow_mut() = client;
                self.ca_loaded.set(true);
            }
        }
        Ok(self.http.borrow().clone())
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The bootstrap key.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The node certificate to trust, when the node serves https.
    pub fn ca_path(&self) -> Option<&std::path::Path> {
        self.ca_path.as_deref()
    }

    fn idem(&self) -> String {
        self.n.set(self.n.get() + 1);
        format!("e2e-{}-{}", self.tag, self.n.get())
    }

    fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        key: Option<&str>,
    ) -> Result<Reply, ConnError> {
        self.send_with(method, path, body, key, None)
    }

    fn send_with(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        key: Option<&str>,
        surface: Option<&str>,
    ) -> Result<Reply, ConnError> {
        let mut rb = self
            .client()?
            .request(method.clone(), format!("{}{path}", self.base))
            .header(
                "Authorization",
                format!("Bearer {}", key.unwrap_or(&self.key)),
            );
        if method != reqwest::Method::GET {
            rb = rb.header("Idempotency-Key", self.idem());
        }
        if let Some(s) = surface {
            rb = rb.header("X-Vam-Surface", s);
        }
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = rb.send().map_err(|e| ConnError(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().map_err(|e| ConnError(e.to_string()))?;
        Ok(Reply { status, body })
    }

    /// One request with a caller-chosen credential. `bearer: None` sends no `Authorization`
    /// header at all (the login route); `idempotency` adds a fresh `Idempotency-Key` header.
    pub fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        bearer: Option<&str>,
        idempotency: bool,
    ) -> Result<Reply, ConnError> {
        let mut rb = self
            .client()?
            .request(method, format!("{}{path}", self.base));
        if let Some(b) = bearer {
            rb = rb.header("Authorization", format!("Bearer {b}"));
        }
        if idempotency {
            rb = rb.header("Idempotency-Key", self.idem());
        }
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = rb.send().map_err(|e| ConnError(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().map_err(|e| ConnError(e.to_string()))?;
        Ok(Reply { status, body })
    }

    pub fn get(&self, path: &str) -> Result<Reply, ConnError> {
        self.send(reqwest::Method::GET, path, None, None)
    }
    pub fn put(&self, path: &str, body: &Value) -> Result<Reply, ConnError> {
        self.send(reqwest::Method::PUT, path, Some(body), None)
    }
    pub fn post(&self, path: &str, body: &Value) -> Result<Reply, ConnError> {
        self.send(reqwest::Method::POST, path, Some(body), None)
    }
    pub fn patch(&self, path: &str, body: &Value) -> Result<Reply, ConnError> {
        self.send(reqwest::Method::PATCH, path, Some(body), None)
    }
    pub fn delete(&self, path: &str) -> Result<Reply, ConnError> {
        self.send(reqwest::Method::DELETE, path, None, None)
    }
    /// DELETE with another identity's key (`None`: the bootstrap key).
    pub fn delete_as(&self, key: Option<&str>, path: &str) -> Result<Reply, ConnError> {
        self.send(reqwest::Method::DELETE, path, None, key)
    }
    /// PUT with another identity's key (`None`: the bootstrap key).
    pub fn put_as(&self, key: Option<&str>, path: &str, body: &Value) -> Result<Reply, ConnError> {
        self.send(reqwest::Method::PUT, path, Some(body), key)
    }
    /// GET with another identity's key (`None`: the bootstrap key).
    pub fn get_as(&self, key: Option<&str>, path: &str) -> Result<Reply, ConnError> {
        self.send(reqwest::Method::GET, path, None, key)
    }
    /// POST with another identity's key (`None`: the bootstrap key).
    pub fn post_as(&self, key: Option<&str>, path: &str, body: &Value) -> Result<Reply, ConnError> {
        self.send(reqwest::Method::POST, path, Some(body), key)
    }
    /// POST as another identity from a surface (`X-Vam-Surface: app`).
    pub fn post_as_surface(
        &self,
        key: Option<&str>,
        path: &str,
        body: &Value,
        surface: &str,
    ) -> Result<Reply, ConnError> {
        self.send_with(reqwest::Method::POST, path, Some(body), key, Some(surface))
    }
    /// GET `path` on the same address over plain `http://`, with no `Authorization` header.
    /// A transport failure is returned as the error (the connection was refused or dropped).
    pub fn probe_plain(&self, path: &str) -> Result<Reply, ConnError> {
        let url = match self.base.strip_prefix("https://") {
            Some(rest) => format!("http://{rest}{path}"),
            None => format!("{}{path}", self.base),
        };
        // No redirects: an HTTP to HTTPS redirect must not be followed into an API answer.
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ConnError(e.to_string()))?;
        let resp = client
            .get(url)
            .timeout(Duration::from_secs(5))
            .send()
            .map_err(|e| ConnError(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().unwrap_or_default();
        Ok(Reply { status, body })
    }
    /// A health probe: true when vamd answers 200.
    pub fn healthy(&self) -> bool {
        let Ok(client) = self.client() else {
            return false;
        };
        client
            .get(format!("{}/v1/health", self.base))
            .timeout(Duration::from_secs(2))
            .send()
            .map(|r| {
                r.status().is_success()
                    && r.text()
                        .map(|t| !t.contains("NOT_SERVING"))
                        .unwrap_or(false)
            })
            .unwrap_or(false)
    }
}

/// One answer of a flood: the status (0 when the transport failed) and the headers a refusal
/// carries.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    pub status: u16,
    pub retry_after: Option<String>,
    pub reason: Option<String>,
}

fn answer_of(r: &reqwest::blocking::Response) -> Answer {
    let h = |name: &str| {
        r.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    Answer {
        status: r.status().as_u16(),
        retry_after: h("retry-after"),
        reason: h("x-vam-rate-limited-reason"),
    }
}

impl Api {
    /// `count` GETs of `path` with `key` (`None`: the bootstrap key), spread over a few threads
    /// that share the client's keep-alive connections, as fast as vamd answers. Answers come
    /// back in no particular order.
    pub fn flood_get(
        &self,
        key: Option<&str>,
        path: &str,
        count: u32,
    ) -> Result<Vec<Answer>, ConnError> {
        let client = self.client()?;
        let url = format!("{}{path}", self.base);
        let bearer = format!("Bearer {}", key.unwrap_or(&self.key));
        let workers = count.clamp(1, 8);
        let mut handles = Vec::new();
        for w in 0..workers {
            // The first workers take the remainder.
            let n = count / workers + u32::from(w < count % workers);
            let (client, url, bearer) = (client.clone(), url.clone(), bearer.clone());
            handles.push(std::thread::spawn(move || {
                (0..n)
                    .map(
                        |_| match client.get(&url).header("Authorization", &bearer).send() {
                            Ok(r) => {
                                let a = answer_of(&r);
                                let _ = r.bytes();
                                a
                            }
                            Err(_) => Answer::default(),
                        },
                    )
                    .collect::<Vec<_>>()
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap_or_default());
        }
        Ok(all)
    }

    /// One GET with `key`, keeping the refusal headers.
    pub fn get_answer(&self, key: Option<&str>, path: &str) -> Result<Answer, ConnError> {
        let r = self
            .client()?
            .get(format!("{}{path}", self.base))
            .header(
                "Authorization",
                format!("Bearer {}", key.unwrap_or(&self.key)),
            )
            .send()
            .map_err(|e| ConnError(e.to_string()))?;
        Ok(answer_of(&r))
    }

    /// POST `path` as multipart with one file field (`file`) holding `bytes`.
    pub fn post_file(
        &self,
        path: &str,
        file_name: &str,
        bytes: Vec<u8>,
    ) -> Result<Reply, ConnError> {
        let part =
            reqwest::blocking::multipart::Part::bytes(bytes).file_name(file_name.to_string());
        let form = reqwest::blocking::multipart::Form::new().part("file", part);
        let resp = self
            .client()?
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.key))
            .header("Idempotency-Key", self.idem())
            .multipart(form)
            .send()
            .map_err(|e| ConnError(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().map_err(|e| ConnError(e.to_string()))?;
        Ok(Reply { status, body })
    }
}

/// Percent-encodes a URN as one path segment (as `vam` does).
pub fn seg(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}
