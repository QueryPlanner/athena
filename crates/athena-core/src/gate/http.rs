//! A minimal HTTP/1.0 client for Athena's own API on the tailnet.
//!
//! The gate only talks plain HTTP to one `IP:port` it read from the env
//! file, and every response it reads is a small JSON body with a
//! `Content-Length`. HTTP/1.0 means the server closes the connection after
//! the response and never uses chunked encoding, so "read to EOF" is the
//! whole protocol. A general client (reqwest's blocking API) would add a
//! second async runtime and TLS stack to a root-run binary for none of
//! that.

use super::sys::{Request, Response};
use std::io::{self, Read, Write};
use std::net::TcpStream;

pub const MAX_RESPONSE_BYTES: u64 = 1 << 20;

pub fn send(request: &Request) -> io::Result<Response> {
    let mut stream = TcpStream::connect_timeout(&request.addr, request.timeout)?;
    stream.set_read_timeout(Some(request.timeout))?;
    stream.set_write_timeout(Some(request.timeout))?;
    stream.write_all(encode(request).as_bytes())?;
    // A compromised staging server must not exhaust a root process's memory.
    let mut raw = Vec::new();
    stream.take(MAX_RESPONSE_BYTES).read_to_end(&mut raw)?;
    decode(&raw)
}

pub fn encode(request: &Request) -> String {
    let body = request.body.as_deref().unwrap_or("");
    let mut text = format!(
        "{} {} HTTP/1.0\r\nHost: {}\r\nContent-Length: {}\r\n",
        request.method,
        request.path,
        request.addr,
        body.len()
    );
    if request.body.is_some() {
        text.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in &request.headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    text.push_str(body);
    text
}

pub fn decode(raw: &[u8]) -> io::Result<Response> {
    let text = String::from_utf8_lossy(raw);
    let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_string());
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| invalid("the response has no end of headers"))?;
    let mut status_line = head.lines().next().unwrap_or_default().split(' ');
    let version = status_line.next().unwrap_or_default();
    let status = status_line
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|_| version.starts_with("HTTP/"))
        .ok_or_else(|| invalid("the response has no HTTP status line"))?;
    Ok(Response {
        status,
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{SocketAddr, TcpListener};
    use std::thread;
    use std::time::Duration;

    fn request(addr: SocketAddr, method: &'static str, body: Option<&str>) -> Request {
        Request {
            method,
            addr,
            path: "/sessions".into(),
            headers: vec![("X-Athena-User", "smoke".into())],
            body: body.map(str::to_string),
            timeout: Duration::from_secs(5),
        }
    }

    /// A one-shot server: returns what the client sent, answers `reply`.
    fn serve_once(reply: &'static str) -> (SocketAddr, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(n) = line.strip_prefix("Content-Length: ") {
                    length = n.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let _ = reader.get_mut().write_all(reply.as_bytes());
            head + &String::from_utf8(body).unwrap()
        });
        (addr, handle)
    }

    #[test]
    fn a_post_sends_host_length_type_and_headers_and_reads_to_eof() {
        let (addr, server) =
            serve_once("HTTP/1.0 201 Created\r\ncontent-length: 10\r\n\r\n{\"id\":\"s\"}");
        let response = send(&request(addr, "POST", Some("{\"name\":\"x\"}"))).unwrap();
        assert_eq!(
            response,
            Response {
                status: 201,
                body: "{\"id\":\"s\"}".into()
            }
        );
        assert_eq!(
            server.join().unwrap(),
            format!(
                "POST /sessions HTTP/1.0\r\nHost: {addr}\r\nContent-Length: 12\r\n\
                 Content-Type: application/json\r\nX-Athena-User: smoke\r\n\r\n{{\"name\":\"x\"}}"
            )
        );
    }

    #[test]
    fn a_get_has_no_body_or_content_type() {
        let (addr, server) = serve_once("HTTP/1.1 200 OK\r\n\r\n{}");
        let response = send(&request(addr, "GET", None)).unwrap();
        assert_eq!(response.status, 200);
        let sent = server.join().unwrap();
        assert!(sent.contains("Content-Length: 0\r\n") && !sent.contains("Content-Type"));
    }

    #[test]
    fn a_response_is_read_up_to_a_cap() {
        let huge: &'static str =
            Box::leak(format!("HTTP/1.0 200 OK\r\n\r\n{}", "x".repeat(2 << 20)).into_boxed_str());
        let (addr, server) = serve_once(huge);
        let response = send(&request(addr, "GET", None)).unwrap();
        assert_eq!(response.body.len() as u64, MAX_RESPONSE_BYTES - 19);
        server.join().unwrap();
    }

    #[test]
    fn a_closed_port_is_an_error() {
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(send(&request(addr, "GET", None)).is_err());
    }

    #[test]
    fn a_response_without_a_status_line_or_header_end_is_invalid() {
        for raw in [
            &b"HTTP/1.0 200 OK\r\n"[..],
            b"garbage\r\n\r\n",
            b"HTTP/1.0 abc\r\n\r\n",
            b"\r\n\r\n",
        ] {
            let err = decode(raw).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{raw:?}");
        }
    }
}
