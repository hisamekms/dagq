//! The client's side of the broker's HTTP: one request per connection over
//! `std::net`, as the server answers (`Connection: close`, the body sized by
//! `Content-Length`). No TLS, no redirects, no proxies: the broker is on
//! loopback only (ADR-t827-2 decision 1).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// The most bytes of an answer the client reads: the server's limits
/// (8 MiB for a request, 4 MiB for fs content, 1 MiB of exec output) with
/// room for JSON's escapes.
pub const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// The most bytes of the answer's head.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// One request.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Headers after `Host`, `Content-Length` and `Connection`.
    pub headers: Vec<(&'a str, String)>,
    pub body: &'a [u8],
}

/// One answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// The value of the header `name` (any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Why an exchange failed before an answer could be read.
#[derive(Debug)]
pub enum ExchangeError {
    /// Connecting, writing or reading failed.
    Io(io::Error),
    /// What came back is no HTTP answer the client reads.
    Malformed(String),
}

/// Send `request` to `addr` and read the whole answer. `connect` bounds the
/// connection and the writing, `read` the wait for the answer.
pub fn exchange(
    addr: SocketAddr,
    request: &Request<'_>,
    connect: Duration,
    read: Duration,
) -> Result<Response, ExchangeError> {
    let mut stream = TcpStream::connect_timeout(&addr, connect).map_err(ExchangeError::Io)?;
    stream
        .set_write_timeout(Some(connect))
        .and_then(|()| stream.set_read_timeout(Some(read)))
        .map_err(ExchangeError::Io)?;
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n",
        request.method,
        request.path,
        request.body.len()
    );
    for (name, value) in &request.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(request.body))
        .and_then(|()| stream.flush())
        .map_err(ExchangeError::Io)?;
    read_response(stream)
}

/// Read an answer from `stream` up to its `Content-Length`, or to the end
/// when it has none.
pub fn read_response(stream: impl Read) -> Result<Response, ExchangeError> {
    let mut raw = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES + MAX_HEAD_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(ExchangeError::Io)?;
    let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(malformed("the answer has no end of its head"));
    };
    if end > MAX_HEAD_BYTES {
        return Err(malformed("the answer's head is too long"));
    }
    let head =
        std::str::from_utf8(&raw[..end]).map_err(|_| malformed("the answer's head is no text"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut words = status_line.splitn(3, ' ');
    let status = match (words.next(), words.next()) {
        (Some(version), Some(code)) if version.starts_with("HTTP/1.") => code
            .parse::<u16>()
            .map_err(|_| malformed("the answer's status is no number"))?,
        _ => return Err(malformed("the answer has no HTTP/1 status line")),
    };
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| malformed("an answer's header has no `:`"))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let mut body = raw.split_off(end + 4);
    let response = Response {
        status,
        headers,
        body: Vec::new(),
    };
    if let Some(length) = response.header("content-length") {
        let length: usize = length
            .parse()
            .map_err(|_| malformed("the answer's Content-Length is no number"))?;
        if body.len() < length {
            return Err(malformed("the answer ended before its Content-Length"));
        }
        body.truncate(length);
    }
    if body.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(malformed("the answer is larger than the client reads"));
    }
    Ok(Response { body, ..response })
}

fn malformed(reason: &str) -> ExchangeError {
    ExchangeError::Malformed(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &str) -> Result<Response, ExchangeError> {
        read_response(raw.as_bytes())
    }

    #[test]
    fn reads_the_status_headers_and_sized_body() {
        let response = read(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nX-Dagq-Broker-Protocol: 1\r\n\r\n{}junk",
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"{}");
        assert_eq!(response.header("x-dagq-broker-protocol"), Some("1"));
        assert_eq!(response.header("X-DAGQ-BROKER-PROTOCOL"), Some("1"));
        assert_eq!(response.header("missing"), None);
        // Without Content-Length the body runs to the end.
        let response = read("HTTP/1.0 401 Unauthorized\r\n\r\nabc").unwrap();
        assert_eq!(
            (response.status, response.body.as_slice()),
            (401, &b"abc"[..])
        );
    }

    #[test]
    fn refuses_what_is_no_answer() {
        for (raw, reason) in [
            ("HTTP/1.1 200 OK\r\n", "no end of its head"),
            ("SSH-2.0 hello\r\n\r\n", "no HTTP/1 status line"),
            ("HTTP/1.1 abc OK\r\n\r\n", "status is no number"),
            ("HTTP/1.1 200 OK\r\nbroken\r\n\r\n", "has no `:`"),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n",
                "Content-Length is no number",
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n{}",
                "ended before its Content-Length",
            ),
        ] {
            match read(raw) {
                Err(ExchangeError::Malformed(message)) => {
                    assert!(message.contains(reason), "{raw:?}: {message}")
                }
                other => panic!("{raw:?}: {other:?}"),
            }
        }
        let long = format!(
            "HTTP/1.1 200 OK\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEAD_BYTES)
        );
        assert!(matches!(read(&long), Err(ExchangeError::Malformed(m)) if m.contains("too long")));
        let not_text = [b"HTTP/1.1 200 \xff\r\n\r\n".as_slice()].concat();
        assert!(matches!(
            read_response(not_text.as_slice()),
            Err(ExchangeError::Malformed(m)) if m.contains("no text")
        ));
    }

    #[test]
    fn a_refused_connection_is_an_io_error() {
        // Bind and drop a listener to find a port nothing listens on.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let request = Request {
            method: "GET",
            path: "/v1/health",
            headers: Vec::new(),
            body: b"",
        };
        let error = exchange(
            addr,
            &request,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(error, ExchangeError::Io(_)), "{error:?}");
    }
}
