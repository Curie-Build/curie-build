//! A tiny HTTP OSGi bundle repository used by tests and the `dummy-repo`
//! subcommand. Accepts GET and PUT; stores files under a directory.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// Bind `127.0.0.1:port` (`0` picks an ephemeral port).
pub fn bind(port: u16) -> Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind dummy OSGi repo on 127.0.0.1:{port}"))
}

/// Serve forever, writing PUT bodies under `dir`.
pub fn serve_forever(listener: TcpListener, dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&dir)?;
    eprintln!(
        "dummy OSGi repository listening on http://{}/  (store {})",
        listener.local_addr()?,
        dir.display()
    );
    for incoming in listener.incoming() {
        let stream = incoming.context("accept failed")?;
        if let Err(e) = handle_connection(stream, &dir) {
            eprintln!("dummy-repo request error: {e:#}");
        }
    }
    Ok(())
}

/// Handle exactly `count` requests then return. Used by unit tests.
#[cfg(test)]
pub fn serve_requests(listener: &TcpListener, dir: &Path, count: usize) -> Result<()> {
    fs::create_dir_all(dir)?;
    for _ in 0..count {
        let (stream, _) = listener.accept().context("accept failed")?;
        handle_connection(stream, dir)?;
    }
    Ok(())
}

fn handle_connection(mut stream: TcpStream, dir: &Path) -> Result<()> {
    let req = read_request(&mut stream)?;
    let rel = sanitize_path(&req.path)?;
    match req.method.as_str() {
        "PUT" => {
            let dest = dir.join(&rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, &req.body)
                .with_context(|| format!("failed to store {}", dest.display()))?;
            write_response(&mut stream, 201, "Created", b"")?;
        }
        "GET" => {
            let dest = dir.join(&rel);
            match fs::read(&dest) {
                Ok(bytes) => write_response(&mut stream, 200, "OK", &bytes)?,
                Err(_) => write_response(&mut stream, 404, "Not Found", b"not found")?,
            }
        }
        other => {
            write_response(
                &mut stream,
                405,
                "Method Not Allowed",
                format!("{other} not allowed").as_bytes(),
            )?;
        }
    }
    Ok(())
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn sanitize_path(path: &str) -> Result<PathBuf> {
    let path = path.split('?').next().unwrap_or(path);
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        bail!("empty path");
    }
    let decoded = url_decode(path);
    let rel = PathBuf::from(&decoded);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("refusing path '{decoded}'");
    }
    Ok(rel)
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            bail!("client closed before completing request");
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            bail!("request headers too large");
        }
    }
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("missing header terminator")?;
    let header_bytes = &buf[..header_end];
    let rest = buf[header_end + 4..].to_vec();
    let header_text = String::from_utf8_lossy(header_bytes);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().context("empty request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = rest;
    while body.len() < content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    Ok(HttpRequest { method, path, body })
}

fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &[u8]) -> Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tempfile::TempDir;

    #[test]
    fn sanitize_rejects_parent_dir() {
        assert!(sanitize_path("/../etc/passwd").is_err());
        assert!(sanitize_path("bundles/foo.jar").is_ok());
    }

    #[test]
    fn dummy_repo_put_and_get() {
        let tmp = TempDir::new().unwrap();
        let listener = bind(0).unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = tmp.path().to_path_buf();
        let store = dir.clone();
        let server = thread::spawn(move || serve_requests(&listener, &store, 2).unwrap());

        let url = format!("http://{addr}/bundles/demo.jar");
        ureq::put(&url).send_bytes(b"bundle-bytes").unwrap();
        let got = ureq::get(&url).call().unwrap().into_string().unwrap();
        assert_eq!(got, "bundle-bytes");
        server.join().unwrap();
        assert_eq!(
            fs::read(dir.join("bundles/demo.jar")).unwrap(),
            b"bundle-bytes"
        );
    }
}
