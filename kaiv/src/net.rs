//! Layer 3/4 network resolution (feature `net`, enabled by default;
//! disable it for embedded/offline builds — the core pipeline has no
//! other dependencies). Registry artifacts are immutable eternalinks,
//! so the on-disk cache never revalidates: a hit is served without
//! touching the network, which also makes `KAIV_OFFLINE=1` (or the
//! CLI `--offline` flag) a complete resolution mode over a warm
//! cache. Layer 3 redirect aliasing is plain HTTP redirect following
//! (ureq's default).

use crate::error::{AppError, PipelineError};
use std::path::{Path, PathBuf};

/// Registry artifacts are small text files; anything bigger is wrong.
const MAX_BODY: u64 = 4 * 1024 * 1024;

fn err() -> PipelineError {
    PipelineError::App(AppError::SchemaResolution)
}

/// The default cache root: `KAIV_CACHE_DIR`, else
/// `$XDG_CACHE_HOME/kaiv`, else `~/.cache/kaiv`.
pub(crate) fn default_cache_root() -> Option<PathBuf> {
    match std::env::var("KAIV_CACHE_DIR") {
        Ok(d) if !d.is_empty() => return Some(PathBuf::from(d)),
        _ => {}
    }
    match std::env::var("XDG_CACHE_HOME") {
        Ok(d) if !d.is_empty() => return Some(PathBuf::from(d).join("kaiv")),
        _ => {}
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("kaiv"))
}

/// `KAIV_OFFLINE` (any non-empty value but `0`): cache-only mode.
pub(crate) fn env_offline() -> bool {
    std::env::var_os("KAIV_OFFLINE").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Cache path of a canonical URL: `{root}/{host}/{path}` (the scheme
/// is dropped — an artifact is identified by host + path). None for a
/// URL whose path could escape the root.
fn cache_path(root: &Path, url: &str) -> Option<PathBuf> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let mut p = root.to_path_buf();
    for seg in rest.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') {
            return None;
        }
        p.push(seg.replace(':', "_"));
    }
    Some(p)
}

/// Fetch `url` through the immutable cache rooted at `root`. Cache
/// writes are best-effort: a read-only cache directory degrades to
/// plain fetching, never to failure.
pub(crate) fn fetch(
    url: &str,
    root: Option<&Path>,
    offline: bool,
) -> Result<Vec<u8>, PipelineError> {
    let cpath = root.and_then(|r| cache_path(r, url));
    if let Some(p) = &cpath {
        if let Ok(bytes) = std::fs::read(p) {
            return Ok(bytes);
        }
    }
    if offline {
        return Err(err());
    }
    let mut res = ureq::get(url).call().map_err(|_| err())?;
    let bytes = res
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|_| err())?;
    if let Some(p) = &cpath {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // A per-writer temp name (pid + process-global counter) keeps
        // write-then-rename atomic across concurrent fetches of the same
        // URL from different processes and different threads — a shared
        // fixed `.part` could otherwise persist a torn write.
        static PART_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = PART_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = p.with_file_name(format!(
            "{}.{}.{}.part",
            p.file_name().and_then(|n| n.to_str()).unwrap_or("dl"),
            std::process::id(),
            seq
        ));
        if std::fs::write(&tmp, &bytes).is_ok() {
            if std::fs::rename(&tmp, p).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        } else {
            // A failed write may leave a partial file; unlike the old
            // fixed name it would never be overwritten — remove it.
            let _ = std::fs::remove_file(&tmp);
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-shot HTTP server: answers each connection from the
    /// route table, then exits when the listener drops.
    fn serve(routes: Vec<(String, String, String)>) -> (String, std::thread::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut hits = 0;
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = head.split(' ').nth(1).unwrap_or("/").to_string();
                hits += 1;
                let (status, extra, body) = routes
                    .iter()
                    .find(|(p, _, _)| *p == path)
                    .map(|(_, st, b)| (st.clone(), String::new(), b.clone()))
                    .unwrap_or_else(|| ("404 Not Found".into(), String::new(), String::new()));
                let (status, extra) = match status.split_once('>') {
                    // "301>/target" — redirect shorthand.
                    Some(("301", to)) => (
                        "301 Moved Permanently".to_string(),
                        format!("Location: {to}\r\n"),
                    ),
                    _ => (status, extra),
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                if hits >= routes.len().max(4) {
                    break; // enough — don't outlive the test
                }
            }
            hits
        });
        (addr, handle)
    }

    fn tmp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("kaiv-net-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn fetch_caches_immutably() {
        let (base, _h) = serve(vec![(
            "/acme/net.taiv".into(),
            "200 OK".into(),
            ".!kaivtype 1 acme/net\n".into(),
        )]);
        let root = tmp_root("cache");
        let url = format!("{base}/acme/net.taiv");
        let a = fetch(&url, Some(&root), false).unwrap();
        assert_eq!(a, b".!kaivtype 1 acme/net\n");
        // Second fetch is served from the cache — works offline.
        let b = fetch(&url, Some(&root), true).unwrap();
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn offline_with_cold_cache_fails() {
        let root = tmp_root("cold");
        let r = fetch("http://127.0.0.1:9/x.taiv", Some(&root), true);
        assert_eq!(r, Err(PipelineError::App(AppError::SchemaResolution)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn redirects_follow() {
        // Layer 3 aliasing: 301 from the alias path to the real one.
        let (base, _h) = serve(vec![
            ("/alias/x.taiv".into(), "301>/real/x.taiv".into(), "".into()),
            ("/real/x.taiv".into(), "200 OK".into(), "!int\n&x=\n".into()),
        ]);
        let got = fetch(&format!("{base}/alias/x.taiv"), None, false).unwrap();
        assert_eq!(got, b"!int\n&x=\n");
    }

    #[test]
    fn cache_paths_are_confined() {
        let root = PathBuf::from("/tmp/kaiv-x");
        assert!(cache_path(&root, "https://h/../etc/passwd").is_none());
        assert!(cache_path(&root, "https://h//x.taiv").is_none());
        assert!(cache_path(&root, "ftp://h/x.taiv").is_none());
        assert_eq!(
            cache_path(&root, "https://h:8080/a/b.taiv").unwrap(),
            root.join("h_8080").join("a").join("b.taiv")
        );
    }
}
