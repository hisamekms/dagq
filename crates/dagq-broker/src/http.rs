//! A small HTTP/1.1 reader and writer over one stream: one request per
//! connection, the body by `Content-Length` only, `Connection: close`. The
//! broker speaks to its own client on loopback, so it needs no more
//! (no async runtime and no HTTP crate; broker.md).

use std::io::{self, BufRead, BufReader, Read, Write};

use dagq_broker_protocol::MAX_REQUEST_BYTES;

/// The longest request head (request line and headers) the server reads.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// One request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// The headers, names in lower case, in order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// The first header named `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Why a request could not be read. The messages hold no header values (the
/// `Authorization` header carries the token).
#[derive(Debug)]
pub enum ReadError {
    /// The peer sent nothing and went away: not a request.
    Empty,
    /// The peer sent part of a request, then went away, stalled past its
    /// deadline or failed.
    Incomplete,
    /// Not a request this server reads: the message says why.
    Bad(&'static str),
}

/// Read one request from `stream`. The head is read up to
/// [`MAX_HEAD_BYTES`] and the body up to [`MAX_REQUEST_BYTES`], growing as
/// the bytes arrive (a `Content-Length` alone allocates nothing).
pub fn read_request(stream: impl Read) -> Result<Request, ReadError> {
    let mut reader = BufReader::new(stream);
    let mut head_bytes = 0;
    let mut line = String::new();
    let mut next_line = |reader: &mut BufReader<_>, line: &mut String| -> Result<(), ReadError> {
        line.clear();
        let room = (MAX_HEAD_BYTES - head_bytes) as u64 + 1;
        let read = reader
            .by_ref()
            .take(room)
            .read_line(line)
            .map_err(|error| match error.kind() {
                io::ErrorKind::InvalidData => ReadError::Bad("the request head is not UTF-8"),
                _ if head_bytes == 0 && line.is_empty() => ReadError::Empty,
                _ => ReadError::Incomplete,
            })?;
        head_bytes += read;
        if head_bytes > MAX_HEAD_BYTES {
            return Err(ReadError::Bad("the request head is too long"));
        }
        if !line.ends_with('\n') {
            return Err(if head_bytes == 0 {
                ReadError::Empty
            } else {
                ReadError::Incomplete
            });
        }
        let trimmed = line.trim_end_matches(['\r', '\n']).len();
        line.truncate(trimmed);
        Ok(())
    };

    next_line(&mut reader, &mut line)?;
    let mut parts = line.split(' ');
    let (Some(method), Some(path), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ReadError::Bad("the request line is malformed"));
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(ReadError::Bad("the HTTP version is not 1.0 or 1.1"));
    }
    let (method, path) = (method.to_owned(), path.to_owned());

    let mut headers = Vec::new();
    loop {
        next_line(&mut reader, &mut line)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(ReadError::Bad("a header is malformed"))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let request = Request {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    if request.header("transfer-encoding").is_some() {
        return Err(ReadError::Bad("a chunked body is not supported"));
    }
    let length = match request.header("content-length") {
        None => 0,
        Some(length) => length
            .parse::<usize>()
            .map_err(|_| ReadError::Bad("the content length is not a number"))?,
    };
    if length > MAX_REQUEST_BYTES {
        return Err(ReadError::Bad("the body is over the limit"));
    }
    let mut body = Vec::with_capacity(length.min(64 * 1024));
    let read = reader.take(length as u64).read_to_end(&mut body);
    if read.is_err() || body.len() < length {
        return Err(ReadError::Incomplete);
    }
    Ok(Request { body, ..request })
}

/// One response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Extra headers after `Content-Type` and `Content-Length`.
    pub headers: Vec<(&'static str, String)>,
    /// JSON.
    pub body: Vec<u8>,
}

/// Write `response` to `stream` and close the exchange.
pub fn write_response(mut stream: impl Write, response: &Response) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        response.status,
        reason(response.status),
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        413 => "Payload Too Large",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &[u8]) -> Result<Request, ReadError> {
        read_request(raw)
    }

    fn bad(raw: &[u8]) -> &'static str {
        match read(raw) {
            Err(ReadError::Bad(reason)) => reason,
            other => panic!("expected a bad request, got {other:?}"),
        }
    }

    #[test]
    fn reads_a_request_with_a_body() {
        let request = read(
            b"POST /v1/fs/read HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nContent-Length: 4\r\n\r\n{}{}",
        )
        .unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/fs/read");
        assert_eq!(request.header("authorization"), Some("Bearer t"));
        assert_eq!(request.header("host"), Some("x"));
        assert_eq!(request.header("missing"), None);
        assert_eq!(request.body, b"{}{}");
    }

    #[test]
    fn reads_a_request_without_a_body_and_bare_newlines() {
        let request = read(b"GET /v1/health HTTP/1.0\nX-A:  b \n\n").unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.header("x-a"), Some("b"));
        assert!(request.body.is_empty());
    }

    #[test]
    fn refuses_what_it_does_not_read() {
        assert_eq!(bad(b"GET /\r\n\r\n"), "the request line is malformed");
        assert_eq!(
            bad(b"GET / HTTP/1.1 x\r\n\r\n"),
            "the request line is malformed"
        );
        assert_eq!(
            bad(b"GET / HTTP/2\r\n\r\n"),
            "the HTTP version is not 1.0 or 1.1"
        );
        assert_eq!(
            bad(b"GET / HTTP/1.1\r\nnocolon\r\n\r\n"),
            "a header is malformed"
        );
        assert_eq!(
            bad(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"),
            "a chunked body is not supported"
        );
        assert_eq!(
            bad(b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n"),
            "the content length is not a number"
        );
        let over = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_REQUEST_BYTES + 1
        );
        assert_eq!(bad(over.as_bytes()), "the body is over the limit");
        let long = format!(
            "GET / HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEAD_BYTES)
        );
        assert_eq!(bad(long.as_bytes()), "the request head is too long");
        assert_eq!(
            bad(b"GET / HTTP/1.1\r\nX: \xff\r\n\r\n"),
            "the request head is not UTF-8"
        );
    }

    #[test]
    fn a_short_stream_is_empty_or_incomplete() {
        assert!(matches!(read(b""), Err(ReadError::Empty)));
        for raw in [
            &b"G"[..],
            b"GET / HTTP/1.1",
            b"GET / HTTP/1.1\r\n",
            b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nab",
        ] {
            assert!(matches!(read(raw), Err(ReadError::Incomplete)), "{raw:?}");
        }
    }

    /// A reader that fails after its bytes, like a socket that timed out.
    struct Failing(&'static [u8]);

    impl Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn a_failing_stream_is_empty_or_incomplete() {
        assert!(matches!(read_request(Failing(b"")), Err(ReadError::Empty)));
        for raw in [
            &b"GET / HT"[..],
            b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nab",
        ] {
            assert!(
                matches!(read_request(Failing(raw)), Err(ReadError::Incomplete)),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn a_line_without_an_end_stops_at_the_head_limit() {
        let mut raw = b"GET / HTTP/1.1\r\nX: ".to_vec();
        raw.extend(std::iter::repeat_n(b'a', 4 * MAX_HEAD_BYTES));
        assert_eq!(bad(&raw), "the request head is too long");
    }

    #[test]
    fn writes_the_status_the_headers_and_the_body() {
        let mut out = Vec::new();
        write_response(
            &mut out,
            &Response {
                status: 401,
                headers: vec![("X-A", "b".to_owned())],
                body: b"{}".to_vec(),
            },
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\nX-A: b\r\nConnection: close\r\n\r\n{}"
        );
        for (status, text) in [
            (200, "OK"),
            (400, "Bad Request"),
            (403, "Forbidden"),
            (413, "Payload Too Large"),
            (502, "Bad Gateway"),
            (504, "Gateway Timeout"),
            (999, "Unknown"),
        ] {
            assert_eq!(reason(status), text);
        }
    }
}
