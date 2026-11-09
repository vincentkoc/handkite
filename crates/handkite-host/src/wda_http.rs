const MAX_HEADER: usize = 64 * 1024;
pub const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpError {
    InvalidRequest,
    InvalidResponse,
    HeaderTooLarge,
    BodyTooLarge,
    MissingFraming,
}

pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub fn request(method: &str, path: &str, body: &[u8]) -> Result<Vec<u8>, HttpError> {
    if !matches!(method, "GET" | "POST" | "DELETE")
        || !path.starts_with('/')
        || path.bytes().any(|b| b.is_ascii_control() || b == b' ')
        || body.len() > MAX_BODY
    {
        return Err(HttpError::InvalidRequest);
    }
    let mut bytes = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    Ok(bytes)
}

/// Return `None` while a bounded response is incomplete.
pub fn parse_response(raw: &[u8]) -> Result<Option<HttpResponse>, HttpError> {
    let split = match raw.windows(4).position(|window| window == b"\r\n\r\n") {
        Some(split) if split <= MAX_HEADER => split,
        Some(_) => return Err(HttpError::HeaderTooLarge),
        None if raw.len() <= MAX_HEADER => return Ok(None),
        None => return Err(HttpError::HeaderTooLarge),
    };
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| HttpError::InvalidResponse)?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| {
            let mut parts = line.split_ascii_whitespace();
            let protocol = parts.next()?;
            let code = parts.next()?;
            (protocol.starts_with("HTTP/1.") && code.len() == 3)
                .then(|| code.parse::<u16>().ok())
                .flatten()
        })
        .ok_or(HttpError::InvalidResponse)?;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(HttpError::InvalidResponse)?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = value
                .parse::<usize>()
                .map_err(|_| HttpError::InvalidResponse)?;
            if length.replace(parsed).is_some() {
                return Err(HttpError::InvalidResponse);
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if !value.eq_ignore_ascii_case("chunked") {
                return Err(HttpError::MissingFraming);
            }
            chunked = true;
        }
    }
    if chunked && length.is_some() {
        return Err(HttpError::InvalidResponse);
    }
    let body = &raw[split + 4..];
    if chunked {
        return decode_chunked(body)
            .map(|decoded| decoded.map(|body| HttpResponse { status, body }));
    }
    let length = length.ok_or(HttpError::MissingFraming)?;
    if length > MAX_BODY {
        return Err(HttpError::BodyTooLarge);
    }
    if body.len() < length {
        return Ok(None);
    }
    Ok(Some(HttpResponse {
        status,
        body: body[..length].to_vec(),
    }))
}

fn decode_chunked(body: &[u8]) -> Result<Option<Vec<u8>>, HttpError> {
    let mut cursor = 0;
    let mut decoded = Vec::new();
    loop {
        let remaining = &body[cursor..];
        let Some(line_end) = remaining.windows(2).position(|window| window == b"\r\n") else {
            return Ok(None);
        };
        if line_end > 32 {
            return Err(HttpError::InvalidResponse);
        }
        let line =
            std::str::from_utf8(&remaining[..line_end]).map_err(|_| HttpError::InvalidResponse)?;
        let hex = line.split(';').next().ok_or(HttpError::InvalidResponse)?;
        let size = usize::from_str_radix(hex, 16).map_err(|_| HttpError::InvalidResponse)?;
        cursor += line_end + 2;
        if size == 0 {
            if body.len() < cursor + 2 {
                return Ok(None);
            }
            if &body[cursor..cursor + 2] != b"\r\n" {
                return Err(HttpError::InvalidResponse);
            }
            return Ok(Some(decoded));
        }
        if size > MAX_BODY - decoded.len() {
            return Err(HttpError::BodyTooLarge);
        }
        if body.len() < cursor + size + 2 {
            return Ok(None);
        }
        decoded.extend_from_slice(&body[cursor..cursor + size]);
        cursor += size;
        if &body[cursor..cursor + 2] != b"\r\n" {
            return Err(HttpError::InvalidResponse);
        }
        cursor += 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_length_and_chunked_responses() {
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(
            parse_response(&fixed[..fixed.len() - 1])
                .unwrap()
                .map(|response| response.body),
            None
        );
        assert_eq!(parse_response(fixed).unwrap().unwrap().body, b"{}");

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap().unwrap().body, b"{}");
    }

    #[test]
    fn rejects_unbounded_or_smuggled_responses() {
        assert!(matches!(
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n"),
            Err(HttpError::BodyTooLarge)
        ));
        assert!(matches!(
            parse_response(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n"
            ),
            Err(HttpError::InvalidResponse)
        ));
        assert_eq!(
            request("GET", "/status\r\nEvil: true", b""),
            Err(HttpError::InvalidRequest)
        );
    }
}
