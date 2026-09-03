//! The RTSP/HTTP control socket.
//!
//! AirPlay senders speak two protocol tokens down one connection: `RTSP/1.0`
//! for the mirroring verbs and `HTTP/1.1` for the (unused here) video/HLS ones.
//! `hyper` can neither parse nor emit `RTSP/1.0` — its parser rejects the
//! version token and its encoder can only write `HTTP/1.x` status lines — so
//! axum is not an option and this is a plain `TcpListener` with a hand-rolled
//! line protocol. `httparse` does the header grind after a four-byte swap of
//! the version token; the original token is remembered and echoed back.

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Ceiling on a single request's headers, mirroring what the reference
/// receivers cap at. Anything larger is a malformed or hostile peer.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Ceiling on a request body. The largest legitimate one is a SETUP plist of a
/// few kilobytes.
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_HEADERS: usize = 48;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Proto {
    Rtsp,
    Http,
}

impl Proto {
    pub fn token(self) -> &'static str {
        match self {
            Proto::Rtsp => "RTSP/1.0",
            Proto::Http => "HTTP/1.1",
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub proto: Proto,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    pub fn is_binary_plist(&self) -> bool {
        self.content_type()
            .is_some_and(|c| c.contains("apple-binary-plist"))
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Close the connection after writing this response.
    pub close: bool,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            status: 200,
            reason: "OK",
            headers: Vec::new(),
            body: Vec::new(),
            close: false,
        }
    }

    pub fn status(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            ..Self::ok()
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.body = body;
        self
    }

    pub fn closing(mut self) -> Self {
        self.close = true;
        self
    }

    fn encode(&self, proto: Proto, cseq: Option<&str>) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + self.body.len());
        out.extend_from_slice(
            format!("{} {} {}\r\n", proto.token(), self.status, self.reason).as_bytes(),
        );
        out.extend_from_slice(
            format!("Server: AirTunes/{}\r\n", super::dnssd::SOURCE_VERSION).as_bytes(),
        );
        if let Some(cseq) = cseq {
            out.extend_from_slice(format!("CSeq: {cseq}\r\n").as_bytes());
        }
        for (k, v) in &self.headers {
            out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        if self.close {
            out.extend_from_slice(b"Connection: close\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

#[derive(Debug)]
pub enum ReadOutcome {
    Request(Request),
    /// Peer closed cleanly.
    Eof,
}

/// Reads one request off the wire, growing `buf` as needed.
pub async fn read_request(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
) -> Result<ReadOutcome, String> {
    let head_len = loop {
        if let Some(n) = find_head_end(buf) {
            break n;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err("request headers exceeded 64 KiB".to_string());
        }
        let mut chunk = [0u8; 4096];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("control socket read failed: {e}"))?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(ReadOutcome::Eof);
            }
            return Err("control socket closed mid-request".to_string());
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    // httparse only accepts HTTP/1.x on the request line. Swap the version
    // token in a scratch copy of the head and remember which one arrived.
    let mut head = buf[..head_len].to_vec();
    let proto = normalize_version(&mut head)?;

    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed.parse(&head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => {
            return Err("request head parsed as incomplete after CRLFCRLF".to_string())
        }
        Err(e) => return Err(format!("malformed request line or headers: {e}")),
    }

    let method = parsed
        .method
        .ok_or_else(|| "request has no method".to_string())?
        .to_string();
    let url = parsed
        .path
        .ok_or_else(|| "request has no URL".to_string())?
        .to_string();

    let mut map = HashMap::new();
    for h in parsed.headers.iter() {
        if h.name.is_empty() {
            continue;
        }
        map.insert(
            h.name.to_ascii_lowercase(),
            String::from_utf8_lossy(h.value).trim().to_string(),
        );
    }

    let content_length: usize = map
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err(format!(
            "request body of {content_length} bytes is implausible"
        ));
    }

    while buf.len() < head_len + content_length {
        let mut chunk = [0u8; 8192];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("control socket read failed: {e}"))?;
        if n == 0 {
            return Err("control socket closed mid-body".to_string());
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let body = buf[head_len..head_len + content_length].to_vec();
    buf.drain(..head_len + content_length);

    Ok(ReadOutcome::Request(Request {
        method,
        url,
        proto,
        headers: map,
        body,
    }))
}

pub async fn write_response(
    stream: &mut TcpStream,
    proto: Proto,
    cseq: Option<&str>,
    response: &Response,
) -> Result<(), String> {
    let bytes = response.encode(proto, cseq);
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| format!("control socket write failed: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("control socket flush failed: {e}"))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Rewrites an `RTSP/1.0` version token to `HTTP/1.1` in place so `httparse`
/// will accept the request line. Only the first line is touched — a body can
/// legitimately contain the same bytes.
fn normalize_version(head: &mut [u8]) -> Result<Proto, String> {
    let eol = head
        .windows(2)
        .position(|w| w == b"\r\n")
        .ok_or_else(|| "request line has no CRLF".to_string())?;
    let line = &mut head[..eol];
    match line.windows(8).position(|w| w == b"RTSP/1.0") {
        Some(at) => {
            line[at..at + 4].copy_from_slice(b"HTTP");
            line[at + 5..at + 8].copy_from_slice(b"1.1");
            Ok(Proto::Rtsp)
        }
        None => Ok(Proto::Http),
    }
}

/// How long a control connection may sit idle before we treat the sender as
/// gone. The sender polls `POST /feedback` every ~2 s, so anything past this is
/// a dead session.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;

    fn head(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn rtsp_version_is_swapped_and_remembered() {
        let mut h = head("SETUP rtsp://10.0.0.5/1234 RTSP/1.0\r\nCSeq: 4\r\n\r\n");
        assert_eq!(normalize_version(&mut h).unwrap(), Proto::Rtsp);
        assert!(h.starts_with(b"SETUP rtsp://10.0.0.5/1234 HTTP/1.1\r\n"));
    }

    #[test]
    fn http_requests_pass_through_untouched() {
        let mut h = head("GET /server-info HTTP/1.1\r\n\r\n");
        assert_eq!(normalize_version(&mut h).unwrap(), Proto::Http);
        assert!(h.starts_with(b"GET /server-info HTTP/1.1\r\n"));
    }

    #[test]
    fn a_body_containing_the_token_is_not_rewritten() {
        // The swap must be bounded to the request line: fp-setup and plist
        // bodies are binary and can contain anything.
        let mut h = head("POST /fp-setup HTTP/1.1\r\nX: RTSP/1.0\r\n\r\n");
        assert_eq!(normalize_version(&mut h).unwrap(), Proto::Http);
        assert!(h.ends_with(b"X: RTSP/1.0\r\n\r\n"));
    }

    #[test]
    fn response_echoes_the_protocol_token_and_cseq() {
        let r = Response::ok();
        let out = r.encode(Proto::Rtsp, Some("7"));
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("RTSP/1.0 200 OK\r\n"), "{text}");
        assert!(text.contains("CSeq: 7\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
    }

    #[test]
    fn head_end_is_found_across_a_split() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n"), None);
    }
}
