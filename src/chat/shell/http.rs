//! Just enough HTTP for a local, single-user shell: one request per
//! connection, small bodies, and Server-Sent Events written straight to the
//! socket so they arrive at once.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};

const MAX_HEAD: usize = 16 << 10;
const MAX_BODY: usize = 1 << 20;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    /// Lowercased names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?
            .split(';')
            .filter_map(|c| c.trim().split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }
}

pub fn read_request(stream: &TcpStream) -> Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else { bail!("bad request line") };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), percent_decode(v)))
        .collect();
    let mut headers = HashMap::new();
    let mut seen = line.len();
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h)?;
        seen += n;
        if n == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if seen > MAX_HEAD {
            bail!("headers too large");
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        bail!("body too large");
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).context("reading the body")?;
    Ok(Request { method: method.to_string(), path: percent_decode(path), query, headers, body })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |j: usize| b.get(j).and_then(|c| (*c as char).to_digit(16));
        match b[i] {
            b'%' => match (hex(i + 1), hex(i + 2)) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    }
}

/// Write a complete response and close.
pub fn respond(mut stream: &TcpStream, status: u16, content_type: &str, body: &[u8], extra: &[(&str, String)]) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\ncache-control: no-store\r\n\
         x-content-type-options: nosniff\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n",
        reason(status),
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn json(stream: &TcpStream, status: u16, value: &serde_json::Value) -> io::Result<()> {
    respond(stream, status, "application/json", value.to_string().as_bytes(), &[])
}

/// Start an event stream; then write events with [`event`].
pub fn start_events(mut stream: &TcpStream) -> io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
    )?;
    stream.flush()
}

pub fn event(mut stream: &TcpStream, data: &str) -> io::Result<()> {
    // One event per line of JSON; JSON has no raw newlines.
    stream.write_all(format!("data: {data}\n\n").as_bytes())?;
    stream.flush()
}

pub fn keepalive(mut stream: &TcpStream) -> io::Result<()> {
    stream.write_all(b": keepalive\n\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    #[test]
    fn parses_a_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(
                b"POST /api/send?token=a%20b HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: x=1; orion_token=abc\r\nContent-Length: 7\r\n\r\n{\"a\":1}",
            )
            .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        });
        let (stream, _) = listener.accept().unwrap();
        let req = read_request(&stream).unwrap();
        assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/api/send"));
        assert_eq!(req.query["token"], "a b");
        assert_eq!(req.cookie("orion_token"), Some("abc"));
        assert_eq!(req.body, br#"{"a":1}"#);
        respond(&stream, 200, "text/plain", b"ok", &[]).unwrap();
        drop(stream);
        let reply = client.join().unwrap();
        assert!(reply.starts_with("HTTP/1.1 200 OK\r\n") && reply.ends_with("\r\n\r\nok"), "{reply}");
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%2Fb+c%"), "a/b c%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
