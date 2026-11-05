//! Talking to a running daemon: one request, one signature, one response.
//!
//! The HTTP client is hand-written on purpose. What it needs is four things — a request line, headers,
//! a body read to end of stream, and a status — over a unix socket that both ends control. A general
//! client would bring TLS, proxies and redirects that this path must not have (a redirect would send a
//! signature somewhere else), and a dependency that exists to write `GET /v1/config HTTP/1.1` is a
//! dependency with a supply chain.
//!
//! `Connection: close` keeps it honest: the server closes the stream when the response is complete, so
//! the body is "everything after the blank line". Chunked encoding is not accepted rather than
//! mishandled — with a closed connection and a known body, our own server does not use it, and a client
//! that silently mis-parses test output is worse than one that says it cannot.

use nucleos::error::{Error, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// One response.
#[derive(Debug)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The body, as text.
    pub body: String,
}

/// Send one signed request and read the answer.
pub fn request(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: &[u8],
    authorization: &str,
    extra: &[(&str, &str)],
) -> Result<Response> {
    let mut stream = UnixStream::connect(socket).map_err(|error| {
        Error::unavailable(format!(
            "cannot reach the daemon at {}: {error}. Is nucleosd running?",
            socket.display()
        ))
    })?;

    // Headers beyond the credential are rare and small: `If-Match` on an apply, nothing else so far.
    let more: String = extra
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let request = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Authorization: {authorization}\r\n\
         Content-Length: {}\r\n\
         {more}\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|error| Error::unavailable(format!("cannot send the request: {error}")))?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|error| Error::unavailable(format!("cannot read the response: {error}")))?;

    parse(&raw)
}

fn parse(raw: &[u8]) -> Result<Response> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| Error::internal("the response has no header block"))?;
    let headers = String::from_utf8_lossy(&raw[..split]);
    let body = raw[split + 4..].to_vec();

    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| Error::internal("the response has no status line"))?;

    let chunked = headers.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("transfer-encoding:") && lower.contains("chunked")
    });
    if chunked {
        // Refused rather than decoded: a client that half-understands framing is a client that
        // reports a truncated body as a parse error somewhere else.
        return Err(Error::internal(
            "the daemon answered with chunked encoding, which this client does not decode",
        ));
    }

    Ok(Response {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_response_is_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"a\":1}";
        let parsed = parse(raw).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body, "{\"a\":1}");
    }

    #[test]
    fn chunked_is_refused_rather_than_half_decoded() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\nab\r\n";
        let error = parse(raw).expect_err("chunked is not decoded");
        assert!(error.reason().contains("chunked"), "{error:?}");
    }

    #[test]
    fn a_response_without_a_header_block_is_an_error() {
        assert!(parse(b"garbage").is_err());
    }
}
