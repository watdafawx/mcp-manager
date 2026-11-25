//! Minimal HTTP/1.1 client for 127.0.0.1 (health probes, the stdio bridge, tests). No TLS,
//! no connection reuse; keeps the bridge process small (it must not load reqwest/TLS).

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

fn io_err(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn head_bytes(
    method: &str,
    path: &str,
    addr: SocketAddr,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut h = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        addr.port()
    );
    for (k, v) in headers {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        h.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    h.push_str("\r\n");
    h.into_bytes()
}

fn parse_head(head: &str) -> io::Result<(u16, Vec<(String, String)>)> {
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io_err("bad status line"))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok((status, headers))
}

/// One request/response; the connection is closed afterwards.
pub async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> io::Result<Response> {
    tokio::time::timeout(timeout, async {
        let mut s = TcpStream::connect(addr).await?;
        s.write_all(&head_bytes(method, path, addr, headers, body))
            .await?;
        if let Some(b) = body {
            s.write_all(b).await?;
        }
        s.flush().await?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).await?;
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| io_err("incomplete response"))?;
        let (status, headers) = parse_head(&String::from_utf8_lossy(&raw[..split]))?;
        let mut body = raw[split + 4..].to_vec();
        if headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        }) {
            body = dechunk(&body)?;
        }
        Ok(Response {
            status,
            headers,
            body,
        })
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "request timed out"))?
}

fn dechunk(mut data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let nl = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| io_err("bad chunk"))?;
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&data[..nl])
                .split(';')
                .next()
                .unwrap_or("")
                .trim(),
            16,
        )
        .map_err(|_| io_err("bad chunk size"))?;
        data = &data[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err(io_err("short chunk"));
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// An open SSE response; yields the `data:` payload of each event.
pub struct EventStream {
    pub status: u16,
    reader: BufReader<TcpStream>,
    chunked: bool,
    buf: String,
    done: bool,
}

pub async fn open_sse(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> io::Result<EventStream> {
    let mut s = TcpStream::connect(addr).await?;
    let mut hs = vec![("Accept", "text/event-stream")];
    hs.extend_from_slice(headers);
    s.write_all(&head_bytes("GET", path, addr, &hs, None))
        .await?;
    s.flush().await?;
    let mut reader = BufReader::new(s);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(io_err("connection closed before headers"));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(&line);
    }
    let (status, hdrs) = parse_head(head.trim_end())?;
    let chunked = hdrs.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    Ok(EventStream {
        status,
        reader,
        chunked,
        buf: String::new(),
        done: false,
    })
}

impl EventStream {
    async fn fill(&mut self) -> io::Result<bool> {
        if self.done {
            return Ok(false);
        }
        if self.chunked {
            let mut line = String::new();
            if self.reader.read_line(&mut line).await? == 0 {
                self.done = true;
                return Ok(false);
            }
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| io_err("bad chunk size"))?;
            if size == 0 {
                self.done = true;
                return Ok(false);
            }
            let mut data = vec![0u8; size + 2];
            self.reader.read_exact(&mut data).await?;
            self.buf.push_str(&String::from_utf8_lossy(&data[..size]));
        } else {
            let mut data = [0u8; 4096];
            let n = self.reader.read(&mut data).await?;
            if n == 0 {
                self.done = true;
                return Ok(false);
            }
            self.buf.push_str(&String::from_utf8_lossy(&data[..n]));
        }
        Ok(true)
    }

    /// Next event that carries data (comments/keep-alives are skipped); `None` at end of stream.
    pub async fn next_event(&mut self) -> io::Result<Option<String>> {
        loop {
            self.buf = self.buf.replace("\r\n", "\n");
            while let Some(end) = self.buf.find("\n\n") {
                let event: String = self.buf.drain(..end + 2).collect();
                let data: Vec<&str> = event
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|d| d.strip_prefix(' ').unwrap_or(d))
                    .collect();
                if !data.is_empty() {
                    return Ok(Some(data.join("\n")));
                }
            }
            if !self.fill().await? {
                return Ok(None);
            }
        }
    }
}
