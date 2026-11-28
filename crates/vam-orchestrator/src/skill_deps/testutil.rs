//! Test helpers: a tiny loopback HTTP server with scripted replies and a request log.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub(crate) struct Req {
    pub head: String,
    pub path: String,
}

pub(crate) struct Reply(Kind);

enum Kind {
    Full {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        chunked: bool,
    },
    Drip {
        body: Vec<u8>,
        gap: Duration,
    },
    Stall {
        prefix: Vec<u8>,
    },
}

impl Reply {
    pub(crate) fn ok(body: Vec<u8>) -> Reply {
        Reply::status(200, &[], &body)
    }

    pub(crate) fn status(code: u16, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        Reply(Kind::Full {
            status: code,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
            chunked: false,
        })
    }

    pub(crate) fn redirect(code: u16, location: &str) -> Reply {
        Reply::status(code, &[("Location", location)], b"")
    }

    /// A body with no Content-Length (chunked), so only a byte counter can cap it.
    pub(crate) fn chunked(body: Vec<u8>) -> Reply {
        Reply(Kind::Full {
            status: 200,
            headers: Vec::new(),
            body,
            chunked: true,
        })
    }

    /// Correct Content-Length, bytes sent one at a time with `gap` between them.
    pub(crate) fn drip(body: Vec<u8>, gap: Duration) -> Reply {
        Reply(Kind::Drip { body, gap })
    }

    /// Promises 1000 bytes, sends `prefix`, then goes quiet.
    pub(crate) fn stall_after(prefix: Vec<u8>) -> Reply {
        Reply(Kind::Stall { prefix })
    }
}

pub(crate) struct TestServer {
    port: u16,
    log: Arc<Mutex<Vec<Req>>>,
    stop: Arc<AtomicBool>,
}

impl TestServer {
    pub(crate) fn start(handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(handler);
        {
            let (log, stop) = (log.clone(), stop.clone());
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(conn) = conn else { continue };
                    let (log, handler) = (log.clone(), handler.clone());
                    std::thread::spawn(move || serve(conn, &log, handler.as_ref()));
                }
            });
        }
        TestServer { port, log, stop }
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn requests(&self) -> Vec<Req> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so the thread exits.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(
    mut conn: TcpStream,
    log: &Mutex<Vec<Req>>,
    handler: &(dyn Fn(&Req) -> Reply + Send + Sync),
) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match conn.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
        if head.len() > 16 * 1024 {
            return;
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let req = Req { head, path };
    if let Ok(mut l) = log.lock() {
        l.push(req.clone());
    }
    let reply = handler(&req);
    let _ = write_reply(&mut conn, reply);
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        404 => "Not Found",
        _ => "Status",
    }
}

fn write_reply(conn: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    match reply.0 {
        Kind::Full {
            status,
            headers,
            body,
            chunked,
        } => {
            let mut out = format!(
                "HTTP/1.1 {status} {}\r\nConnection: close\r\n",
                reason(status)
            );
            for (k, v) in &headers {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            if chunked {
                out.push_str("Transfer-Encoding: chunked\r\n\r\n");
                conn.write_all(out.as_bytes())?;
                for piece in body.chunks(1000) {
                    conn.write_all(format!("{:x}\r\n", piece.len()).as_bytes())?;
                    conn.write_all(piece)?;
                    conn.write_all(b"\r\n")?;
                }
                conn.write_all(b"0\r\n\r\n")?;
            } else {
                out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
                conn.write_all(out.as_bytes())?;
                conn.write_all(&body)?;
            }
        }
        Kind::Drip { body, gap } => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            conn.write_all(head.as_bytes())?;
            conn.flush()?;
            for b in body {
                std::thread::sleep(gap);
                conn.write_all(&[b])?;
                conn.flush()?;
            }
        }
        Kind::Stall { prefix } => {
            conn.write_all(
                b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 1000\r\n\r\n",
            )?;
            conn.write_all(&prefix)?;
            conn.flush()?;
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    conn.flush()
}
