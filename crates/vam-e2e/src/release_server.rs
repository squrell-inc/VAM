//! The `release-server` world piece: a loopback, GitHub-like `releases` API and the assets of one
//! release, made from a fixture directory. No network. It counts the requests it answers, so a
//! scenario can prove the CLI never calls out unasked.
//!
//! Routes (base = `{{url:<alias>}}`, as GitHub's, under `/repos/squrell-inc/VAM`):
//! `GET /repos/squrell-inc/VAM/releases` (all, newest first), `.../releases/latest` (the newest
//! release that is not a pre-release, else 404), `.../releases/tags/<tag>`, and
//! `GET /assets/<name>` (the archive and `SHA256SUMS`). The archive is
//! `vam-<tag without v>-<host target>.tar.gz` (as `package.sh` names it) holding the fixture
//! directory's files (mode 0755 for `vam`);
//! `SHA256SUMS` is computed with `sha256sum` (or `shasum -a 256`), and `tamper: sum` changes one
//! hash digit.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// A running server. Dropping it (or `stop`) ends the accept loop.
#[derive(Debug)]
pub struct ReleaseServer {
    pub base: String,
    pub requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl ReleaseServer {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    pub fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for ReleaseServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The target triple the archive is named for (the one `vam` is built for on this host).
pub fn host_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", _) => "x86_64-apple-darwin",
        (_, "aarch64") => "aarch64-unknown-linux-gnu",
        _ => "x86_64-unknown-linux-gnu",
    }
}

/// `sha256sum` of a file as lowercase hex.
fn sha256(path: &Path) -> Result<String, String> {
    for (prog, args) in [("sha256sum", &[][..]), ("shasum", &["-a", "256"][..])] {
        if let Ok(o) = std::process::Command::new(prog)
            .args(args)
            .arg(path)
            .output()
        {
            if o.status.success() {
                let t = String::from_utf8_lossy(&o.stdout);
                if let Some(h) = t.split_whitespace().next() {
                    return Ok(h.to_string());
                }
            }
        }
    }
    Err("neither `sha256sum` nor `shasum` could hash the archive".into())
}

/// Flips the first hex digit, so the line is a valid hash that matches nothing.
fn wrong(hash: &str) -> String {
    let mut c: Vec<char> = hash.chars().collect();
    c[0] = if c[0] == '0' { '1' } else { '0' };
    c.into_iter().collect()
}

/// Builds the assets under `dir` and starts the server.
pub fn start(
    fixture: &Path,
    dir: &Path,
    tag: &str,
    prerelease: bool,
    tamper_sum: bool,
) -> Result<ReleaseServer, String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // As `package.sh` names it: the version without the tag's `v` (`vam-0.1.0-<triple>.tar.gz`).
    let archive = format!(
        "vam-{}-{}.tar.gz",
        tag.strip_prefix('v').unwrap_or(tag),
        host_target()
    );
    let path = dir.join(&archive);
    {
        let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut names: Vec<_> = std::fs::read_dir(fixture)
            .map_err(|e| format!("{}: {e}", fixture.display()))?
            .flatten()
            .collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let body = std::fs::read(e.path()).map_err(|e| e.to_string())?;
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o755);
            h.set_mtime(0);
            h.set_cksum();
            tar.append_data(&mut h, e.file_name(), body.as_slice())
                .map_err(|e| e.to_string())?;
        }
        tar.into_inner()
            .and_then(|g| g.finish())
            .map_err(|e| e.to_string())?;
    }
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    let mut hash = sha256(&path)?;
    if tamper_sum {
        hash = wrong(&hash);
    }
    std::fs::write(dir.join("SHA256SUMS"), format!("{hash}  {archive}\n"))
        .map_err(|e| e.to_string())?;

    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let base = format!(
        "http://{}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    let assets: Vec<serde_json::Value> = [archive.clone(), "SHA256SUMS".to_string()]
        .iter()
        .map(|n| {
            serde_json::json!({
                "name": n,
                "browser_download_url": format!("{base}/assets/{n}"),
                "size": std::fs::metadata(dir.join(n)).map(|m| m.len()).unwrap_or(0),
            })
        })
        .collect();
    let release = serde_json::json!({
        "tag_name": tag,
        "name": tag,
        "prerelease": prerelease,
        "draft": false,
        "assets": assets,
    });
    let requests = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (rq, st, root, tag) = (
        requests.clone(),
        stop.clone(),
        dir.to_path_buf(),
        tag.to_string(),
    );
    std::thread::spawn(move || {
        while !st.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((s, _)) => {
                    rq.fetch_add(1, Ordering::SeqCst);
                    let _ = serve(s, &release, &tag, prerelease, &root);
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    });
    Ok(ReleaseServer {
        base,
        requests,
        stop,
    })
}

fn respond(s: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = if code == 200 { "OK" } else { "Not Found" };
    write!(
        s,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(body)
}

fn serve(
    mut s: TcpStream,
    release: &serde_json::Value,
    tag: &str,
    prerelease: bool,
    dir: &Path,
) -> std::io::Result<()> {
    s.set_nonblocking(false)?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    // Read the whole request head: one read() can return part of it, and answering a partial
    // request then closing with bytes unread makes the kernel reset the connection.
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 64 * 1024 {
        let n = s.read(&mut buf)?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let req = String::from_utf8_lossy(&head).into_owned();
    let mut parts = req.lines().next().unwrap_or_default().split_whitespace();
    let (method, path) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    let path = path.split('?').next().unwrap_or_default();
    let api = "/repos/squrell-inc/VAM/releases";
    let json = "application/json";
    if method != "GET" {
        return respond(&mut s, 404, json, b"{}");
    }
    if path == api {
        return respond(
            &mut s,
            200,
            json,
            serde_json::json!([release]).to_string().as_bytes(),
        );
    }
    if path == format!("{api}/latest") && !prerelease {
        return respond(&mut s, 200, json, release.to_string().as_bytes());
    }
    if path == format!("{api}/tags/{tag}") {
        return respond(&mut s, 200, json, release.to_string().as_bytes());
    }
    if let Some(name) = path.strip_prefix("/assets/") {
        if !name.contains('/') && !name.contains("..") {
            if let Ok(b) = std::fs::read(dir.join(name)) {
                return respond(&mut s, 200, "application/octet-stream", &b);
            }
        }
    }
    respond(&mut s, 404, json, b"{\"message\":\"Not Found\"}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(base: &str, path: &str) -> (u16, Vec<u8>) {
        let mut s = TcpStream::connect(base.trim_start_matches("http://")).unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        let at = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let code = String::from_utf8_lossy(&out[..12])[9..12].parse().unwrap();
        (code, out[at..].to_vec())
    }

    fn fixture(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("rs-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fx = root.join("fx");
        std::fs::create_dir_all(&fx).unwrap();
        std::fs::write(fx.join("vam"), "#!/bin/sh\necho hi\n").unwrap();
        (fx, root.join("assets"))
    }

    #[test]
    fn it_serves_the_listing_the_archive_and_the_sums_and_counts_requests() {
        let (fx, dir) = fixture("ok");
        let srv = start(&fx, &dir, "v9.9.9", true, false).unwrap();
        let api = "/repos/squrell-inc/VAM/releases";
        let (c, b) = get(&srv.base, api);
        assert_eq!(c, 200);
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v[0]["tag_name"], "v9.9.9");
        assert_eq!(get(&srv.base, &format!("{api}/latest")).0, 404);
        assert_eq!(get(&srv.base, &format!("{api}/tags/v9.9.9")).0, 200);
        let name = format!("vam-9.9.9-{}.tar.gz", host_target());
        let (c, archive) = get(&srv.base, &format!("/assets/{name}"));
        assert_eq!(c, 200);
        assert_eq!(archive, std::fs::read(dir.join(&name)).unwrap());
        let (_, sums) = get(&srv.base, "/assets/SHA256SUMS");
        let want = sha256(&dir.join(&name)).unwrap();
        assert_eq!(
            String::from_utf8(sums).unwrap(),
            format!("{want}  {name}\n")
        );
        assert_eq!(srv.count(), 5);
        assert_eq!(get(&srv.base, "/assets/../x").0, 404);
        srv.stop();
    }

    #[test]
    fn a_tampered_server_lists_a_wrong_hash_and_a_final_is_latest() {
        let (fx, dir) = fixture("bad");
        let srv = start(&fx, &dir, "v9.9.9", false, true).unwrap();
        let name = format!("vam-9.9.9-{}.tar.gz", host_target());
        let (_, sums) = get(&srv.base, "/assets/SHA256SUMS");
        let real = sha256(&dir.join(&name)).unwrap();
        let line = String::from_utf8(sums).unwrap();
        assert!(!line.contains(&real));
        assert!(line.ends_with(&format!("  {name}\n")));
        assert_eq!(
            get(&srv.base, "/repos/squrell-inc/VAM/releases/latest").0,
            200
        );
        srv.stop();
    }
}
