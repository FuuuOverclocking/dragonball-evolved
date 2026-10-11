use std::io;

use http::header::{
    CONNECTION, CONTENT_LENGTH, EXPECT, HOST, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Version};
use tokio::net::UnixStream;

use crate::{Body, Fds, Limits, MAX_HEADERS, invalid, transport};

const X_FD: HeaderName = HeaderName::from_static("x-fd");

pub(crate) struct Connection {
    pub(crate) stream: UnixStream,
    read: Vec<u8>,
    write: Vec<u8>,
    limits: Limits,
}

pub(crate) enum Incoming<'a> {
    Request(&'a mut Request<Body>),
    Response(&'a mut Response<Body>),
}

impl Incoming<'_> {
    fn headers(&self) -> &HeaderMap {
        match self {
            Self::Request(message) => message.headers(),
            Self::Response(message) => message.headers(),
        }
    }

    fn body_mut(&mut self) -> &mut Body {
        match self {
            Self::Request(message) => message.body_mut(),
            Self::Response(message) => message.body_mut(),
        }
    }

    fn body_length(&self, declared: Option<usize>) -> io::Result<usize> {
        match self {
            Self::Request(_) => Ok(declared.unwrap_or(0)),
            Self::Response(message) => incoming_response_body_length(message.status(), declared),
        }
    }
}

impl Connection {
    pub(crate) fn new(stream: UnixStream, limits: Limits) -> Self {
        Self {
            stream,
            limits,
            read: vec![0; 1024],
            write: Vec::with_capacity(1024),
        }
    }

    pub(crate) fn prepare_request(
        &mut self,
        parts: &http::request::Parts,
        body: &mut Body,
    ) -> io::Result<bool> {
        validate_request(&parts.method, &parts.uri, parts.version)?;
        let info = inspect_headers(&parts.headers, true)?;
        if info.length.is_some_and(|length| length != body.len()) {
            return Err(invalid("invalid Content-Length for outgoing request"));
        }
        self.write.clear();
        self.write
            .extend_from_slice(parts.method.as_str().as_bytes());
        self.write.push(b' ');
        self.write.extend_from_slice(
            parts
                .uri
                .path_and_query()
                .map_or(parts.uri.path(), |value| value.as_str())
                .as_bytes(),
        );
        self.write.extend_from_slice(b" HTTP/1.1\r\n");
        let count = self.finish_head(&parts.headers, body, &info, Some(body.len()), false)?;
        if !info.has_host {
            self.write.extend_from_slice(b"host: localhost\r\n");
        }
        self.end_head(count + usize::from(!info.has_host))?;
        Ok(info.close)
    }

    pub(crate) fn prepare_response(
        &mut self,
        parts: &http::response::Parts,
        body: &mut Body,
        close: bool,
    ) -> io::Result<bool> {
        validate_response(parts.status, parts.version)?;
        let info = inspect_headers(&parts.headers, false)?;
        let content_length =
            outgoing_response_content_length(parts.status, body.len(), info.length)?;
        self.write.clear();
        self.write.extend_from_slice(b"HTTP/1.1 ");
        self.write
            .extend_from_slice(parts.status.as_str().as_bytes());
        self.write.push(b' ');
        self.write
            .extend_from_slice(parts.status.canonical_reason().unwrap_or("").as_bytes());
        self.write.extend_from_slice(b"\r\n");
        let close = close || info.close;
        let count = self.finish_head(&parts.headers, body, &info, content_length, close)?;
        self.end_head(count)?;
        Ok(close)
    }

    fn finish_head(
        &mut self,
        headers: &HeaderMap,
        body: &mut Body,
        info: &HeaderInfo<'_>,
        content_length: Option<usize>,
        close: bool,
    ) -> io::Result<usize> {
        if body.len() > self.limits.max_body_bytes {
            return Err(invalid("body exceeds the permitted length"));
        }
        if (0..body.fds().len()).any(|index| body.fds().get(index).is_none()) {
            return Err(invalid("cannot send vacant FD slots"));
        }
        if let Some(names) = info.names {
            body.fds_mut().bind_names(names)?;
        }
        let mut count = 0;
        for (name, value) in headers {
            if name == CONTENT_LENGTH || name == X_FD || (close && name == CONNECTION) {
                continue;
            }
            self.write.extend_from_slice(name.as_str().as_bytes());
            self.write.extend_from_slice(b": ");
            self.write.extend_from_slice(value.as_bytes());
            self.write.extend_from_slice(b"\r\n");
            count += 1;
            if self.write.len() > self.limits.max_header_bytes {
                return Err(invalid("HTTP header too large"));
            }
        }
        if let Some(length) = content_length {
            self.write.extend_from_slice(b"content-length: ");
            push_usize(&mut self.write, length);
            self.write.extend_from_slice(b"\r\n");
            count += 1;
        }
        if close {
            self.write.extend_from_slice(b"connection: close\r\n");
            count += 1;
        }
        if body.fds().name(0).is_some() {
            self.write.extend_from_slice(b"x-fd: ");
            for index in 0..body.fds().len() {
                if index > 0 {
                    self.write.extend_from_slice(b", ");
                }
                self.write
                    .extend_from_slice(body.fds().name(index).unwrap().as_bytes());
            }
            self.write.extend_from_slice(b"\r\n");
            count += 1;
        }
        Ok(count)
    }

    fn end_head(&mut self, count: usize) -> io::Result<()> {
        self.write.extend_from_slice(b"\r\n");
        if count > MAX_HEADERS || self.write.len() > self.limits.max_header_bytes {
            return Err(invalid("HTTP header exceeds configured limits"));
        }
        Ok(())
    }

    pub(crate) async fn send(&self, body: &Body) -> io::Result<()> {
        transport::send(&self.stream, &self.write, body).await
    }

    pub(crate) async fn receive(&mut self, mut head: Incoming<'_>) -> io::Result<Option<bool>> {
        let request = matches!(head, Incoming::Request(_));
        let mut filled = 0;
        let mut fds = Fds::default();
        let end = loop {
            let mut next_fds = Fds::default();
            let n =
                transport::receive(&self.stream, &mut self.read[filled..], &mut next_fds).await?;
            if n == 0 {
                return if filled == 0 && next_fds.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::from(io::ErrorKind::UnexpectedEof))
                };
            }
            if filled == 0 {
                fds = next_fds;
            } else if !next_fds.is_empty() {
                return Err(invalid(
                    "descriptors must accompany the first message bytes",
                ));
            }
            filled += n;
            if let Some(end) = parse_head(&self.read[..filled], &mut head)? {
                if end > self.limits.max_header_bytes {
                    return Err(invalid("HTTP header too large"));
                }
                break end;
            }
            if filled >= self.limits.max_header_bytes {
                return Err(invalid("HTTP header too large"));
            }
            if filled == self.read.len() {
                self.read.resize(
                    filled.saturating_mul(2).min(self.limits.max_header_bytes),
                    0,
                );
            }
        };
        let info = inspect_headers(head.headers(), request)?;
        if request && !info.has_host {
            return Err(invalid("HTTP/1.1 requires one nonempty Host header"));
        }
        let close = info.close;
        let length = head.body_length(info.length)?;
        if length > self.limits.max_body_bytes {
            return Err(invalid("HTTP body too large"));
        }
        let received = filled - end;
        if received > length {
            return Err(invalid("pipelining or trailing bytes are not supported"));
        }
        if let Some(names) = info.names {
            fds.bind_names(names)?;
        }
        let body = head.body_mut();
        *body.fds_mut() = fds;
        body.resize(length);
        body.bytes_mut()[..received].copy_from_slice(&self.read[end..filled]);
        let mut offset = received;
        while offset < length {
            let mut unexpected = Fds::default();
            let n = transport::receive(
                &self.stream,
                &mut body.bytes_mut()[offset..],
                &mut unexpected,
            )
            .await?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            if !unexpected.is_empty() {
                return Err(invalid(
                    "descriptors must accompany the first message bytes",
                ));
            }
            offset += n;
        }
        Ok(Some(close))
    }
}

fn push_usize(output: &mut Vec<u8>, mut value: usize) {
    let mut digits = [0; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    output.extend_from_slice(&digits[start..]);
}

fn parse_head(bytes: &[u8], head: &mut Incoming<'_>) -> io::Result<Option<usize>> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    match head {
        Incoming::Request(message) => {
            let mut parsed = httparse::Request::new(&mut headers);
            let end = match parsed
                .parse(bytes)
                .map_err(|_| invalid("invalid HTTP request head"))?
            {
                httparse::Status::Partial => return Ok(None),
                httparse::Status::Complete(end) => end,
            };
            if parsed.version != Some(1) {
                return Err(invalid("only HTTP/1.1 is supported"));
            }
            *message.method_mut() = Method::from_bytes(parsed.method.unwrap().as_bytes())
                .map_err(|_| invalid("invalid HTTP method"))?;
            *message.uri_mut() = parsed
                .path
                .unwrap()
                .parse()
                .map_err(|_| invalid("invalid request URI"))?;
            *message.version_mut() = Version::HTTP_11;
            validate_request(message.method(), message.uri(), message.version())?;
            materialize_headers(parsed.headers, message.headers_mut())?;
            Ok(Some(end))
        }
        Incoming::Response(message) => {
            let mut parsed = httparse::Response::new(&mut headers);
            let end = match parsed
                .parse(bytes)
                .map_err(|_| invalid("invalid HTTP response head"))?
            {
                httparse::Status::Partial => return Ok(None),
                httparse::Status::Complete(end) => end,
            };
            if parsed.version != Some(1) {
                return Err(invalid("only HTTP/1.1 is supported"));
            }
            *message.status_mut() = StatusCode::from_u16(parsed.code.unwrap())
                .map_err(|_| invalid("invalid status code"))?;
            *message.version_mut() = Version::HTTP_11;
            validate_response(message.status(), message.version())?;
            materialize_headers(parsed.headers, message.headers_mut())?;
            Ok(Some(end))
        }
    }
}

fn materialize_headers(headers: &[httparse::Header<'_>], map: &mut HeaderMap) -> io::Result<()> {
    map.reserve(headers.len());
    for header in headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| invalid("invalid header name"))?;
        let value =
            HeaderValue::from_bytes(header.value).map_err(|_| invalid("invalid header value"))?;
        map.append(name, value);
    }
    Ok(())
}

fn validate_request(method: &Method, uri: &http::Uri, version: Version) -> io::Result<()> {
    if version != Version::HTTP_11 || method == Method::HEAD || method == Method::CONNECT {
        return Err(invalid("unsupported HTTP version or method"));
    }
    if uri.scheme().is_some() || uri.authority().is_some() || !uri.path().starts_with('/') {
        return Err(invalid("request URI must be an absolute path"));
    }
    Ok(())
}

fn validate_response(status: StatusCode, version: Version) -> io::Result<()> {
    if version != Version::HTTP_11 || status.is_informational() {
        return Err(invalid(
            "unsupported HTTP version or informational response",
        ));
    }
    Ok(())
}

fn outgoing_response_content_length(
    status: StatusCode,
    body_len: usize,
    declared: Option<usize>,
) -> io::Result<Option<usize>> {
    match status {
        StatusCode::NO_CONTENT => {
            if body_len != 0 || declared.is_some() {
                return Err(invalid(
                    "204 responses cannot contain a body or Content-Length",
                ));
            }
            Ok(None)
        }
        StatusCode::NOT_MODIFIED => {
            if body_len != 0 {
                return Err(invalid("304 responses cannot contain a body"));
            }
            Ok(declared)
        }
        StatusCode::RESET_CONTENT => {
            if body_len != 0 || declared.is_some_and(|length| length != 0) {
                return Err(invalid("205 responses require an empty body"));
            }
            Ok(Some(0))
        }
        _ => {
            if declared.is_some_and(|length| length != body_len) {
                return Err(invalid("invalid Content-Length for outgoing response"));
            }
            Ok(Some(body_len))
        }
    }
}

fn incoming_response_body_length(status: StatusCode, declared: Option<usize>) -> io::Result<usize> {
    match status {
        StatusCode::NO_CONTENT => {
            if declared.is_some() {
                return Err(invalid("204 responses cannot contain Content-Length"));
            }
            Ok(0)
        }
        StatusCode::NOT_MODIFIED => Ok(0),
        StatusCode::RESET_CONTENT => match declared {
            Some(0) => Ok(0),
            _ => Err(invalid("205 responses require Content-Length: 0")),
        },
        _ => declared.ok_or_else(|| invalid("Content-Length is required for responses")),
    }
}

#[derive(Default)]
struct HeaderInfo<'a> {
    length: Option<usize>,
    names: Option<&'a str>,
    close: bool,
    has_host: bool,
}

fn inspect_headers(headers: &HeaderMap, request: bool) -> io::Result<HeaderInfo<'_>> {
    if headers.len() > MAX_HEADERS {
        return Err(invalid("too many HTTP headers"));
    }
    let mut info = HeaderInfo::default();
    for (name, value) in headers {
        if name == CONTENT_LENGTH {
            if info.length.is_some() {
                return Err(invalid("duplicate framing or FD header"));
            }
            info.length = Some(content_length(value.as_bytes())?);
        } else if name == X_FD {
            if info.names.is_some() {
                return Err(invalid("duplicate framing or FD header"));
            }
            info.names = Some(value.to_str().map_err(|_| invalid("invalid X-FD header"))?);
        } else if name == CONNECTION {
            let value = value
                .to_str()
                .map_err(|_| invalid("invalid Connection header"))?;
            for token in value.split(',').map(str::trim) {
                if token.eq_ignore_ascii_case("close") {
                    info.close = true;
                } else if !token.eq_ignore_ascii_case("keep-alive") {
                    return Err(invalid("unsupported Connection option"));
                }
            }
        } else if name == HOST && request {
            if info.has_host
                || value
                    .as_bytes()
                    .iter()
                    .all(|byte| matches!(byte, b' ' | b'\t'))
            {
                return Err(invalid("HTTP/1.1 requires one nonempty Host header"));
            }
            info.has_host = true;
        } else if matches!(
            name,
            &TRANSFER_ENCODING | &TE | &EXPECT | &UPGRADE | &TRAILER
        ) {
            return Err(invalid(
                "transfer coding, upgrades, expectations and trailers are unsupported",
            ));
        }
    }
    Ok(info)
}

fn content_length(bytes: &[u8]) -> io::Result<usize> {
    if bytes.is_empty() {
        return Err(invalid("empty Content-Length"));
    }
    let mut length = 0usize;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return Err(invalid("invalid Content-Length"));
        }
        length = length
            .checked_mul(10)
            .and_then(|n| n.checked_add((byte - b'0') as usize))
            .ok_or_else(|| invalid("Content-Length overflow"))?;
    }
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::push_usize;

    #[test]
    fn writes_decimal_usize() {
        for value in [0, 1, 9, 10, 99, 100, 1024, usize::MAX] {
            let mut output = Vec::new();
            push_usize(&mut output, value);
            assert_eq!(output, value.to_string().as_bytes());
        }
    }
}
