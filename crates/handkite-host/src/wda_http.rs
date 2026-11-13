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

enum Framing {
    Length(usize),
    Chunked,
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
    ResponseDecoder::new().push(raw)
}

fn parse_head(head: &[u8]) -> Result<(u16, Framing), HttpError> {
    let head = std::str::from_utf8(head).map_err(|_| HttpError::InvalidResponse)?;
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
    if chunked {
        return Ok((status, Framing::Chunked));
    }
    let length = length.ok_or(HttpError::MissingFraming)?;
    if length > MAX_BODY {
        return Err(HttpError::BodyTooLarge);
    }
    Ok((status, Framing::Length(length)))
}

#[derive(Clone, Copy)]
enum DecodeState {
    Headers,
    Length(usize),
    ChunkSize,
    ChunkBody(usize),
    Delimiter { read: usize, last: bool },
    Finished,
}

/// Consumes new wire bytes once; retains only framing and the decoded body.
pub(crate) struct ResponseDecoder {
    state: DecodeState,
    framing: Vec<u8>,
    body: Vec<u8>,
    status: u16,
}

impl ResponseDecoder {
    pub(crate) fn new() -> Self {
        Self {
            state: DecodeState::Headers,
            framing: Vec::new(),
            body: Vec::new(),
            status: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Option<HttpResponse>, HttpError> {
        let result = self.consume(bytes);
        if result.is_err() {
            self.state = DecodeState::Finished;
        }
        result
    }

    fn finish(&mut self) -> HttpResponse {
        self.state = DecodeState::Finished;
        HttpResponse {
            status: self.status,
            body: std::mem::take(&mut self.body),
        }
    }

    fn consume(&mut self, mut bytes: &[u8]) -> Result<Option<HttpResponse>, HttpError> {
        loop {
            match self.state {
                DecodeState::Headers | DecodeState::ChunkSize => {
                    let Some((&byte, rest)) = bytes.split_first() else {
                        return Ok(None);
                    };
                    bytes = rest;
                    self.framing.push(byte);
                    if matches!(self.state, DecodeState::Headers) {
                        if self.framing.len() > MAX_HEADER + 4 {
                            return Err(HttpError::HeaderTooLarge);
                        }
                        if self.framing.ends_with(b"\r\n\r\n") {
                            let (status, framing) =
                                parse_head(&self.framing[..self.framing.len() - 4])?;
                            self.status = status;
                            self.state = match framing {
                                Framing::Length(length) => DecodeState::Length(length),
                                Framing::Chunked => DecodeState::ChunkSize,
                            };
                            self.framing.clear();
                        }
                    } else {
                        if self.framing.len() > 34 {
                            return Err(HttpError::InvalidResponse);
                        }
                        if self.framing.ends_with(b"\r\n") {
                            let line = std::str::from_utf8(&self.framing[..self.framing.len() - 2])
                                .map_err(|_| HttpError::InvalidResponse)?;
                            let size =
                                usize::from_str_radix(line.split(';').next().unwrap_or(""), 16)
                                    .map_err(|_| HttpError::InvalidResponse)?;
                            if size > MAX_BODY - self.body.len() {
                                return Err(HttpError::BodyTooLarge);
                            }
                            self.state = if size == 0 {
                                DecodeState::Delimiter {
                                    read: 0,
                                    last: true,
                                }
                            } else {
                                DecodeState::ChunkBody(size)
                            };
                            self.framing.clear();
                        }
                    }
                }
                DecodeState::Length(0) => return Ok(Some(self.finish())),
                DecodeState::ChunkBody(0) => {
                    self.state = DecodeState::Delimiter {
                        read: 0,
                        last: false,
                    };
                }
                DecodeState::Length(remaining) | DecodeState::ChunkBody(remaining) => {
                    if bytes.is_empty() {
                        return Ok(None);
                    }
                    let count = remaining.min(bytes.len());
                    self.body.extend_from_slice(&bytes[..count]);
                    bytes = &bytes[count..];
                    self.state = if matches!(self.state, DecodeState::Length(_)) {
                        DecodeState::Length(remaining - count)
                    } else {
                        DecodeState::ChunkBody(remaining - count)
                    };
                }
                DecodeState::Delimiter { read, last } => {
                    let Some((&byte, rest)) = bytes.split_first() else {
                        return Ok(None);
                    };
                    bytes = rest;
                    if byte != b"\r\n"[read] {
                        return Err(HttpError::InvalidResponse);
                    }
                    if read == 0 {
                        self.state = DecodeState::Delimiter { read: 1, last };
                    } else if last {
                        return Ok(Some(self.finish()));
                    } else {
                        self.state = DecodeState::ChunkSize;
                    }
                }
                DecodeState::Finished => return Err(HttpError::InvalidResponse),
            }
        }
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

    #[test]
    fn every_fragment_boundary_preserves_length_and_chunked_bodies() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\na\r\n\0bc", b"a\r\n\0bc"),
            (b"HTTP/1.1 201 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n3;ext=1\r\ncde\r\n0\r\n\r\n", b"abcde"),
            (b"HTTP/1.1 204 OK\r\nContent-Length: 0\r\n\r\n", b""),
        ];
        for (wire, expected) in cases {
            for split in 0..=wire.len() {
                let mut decoder = ResponseDecoder::new();
                let result = decoder.push(&wire[..split]).unwrap();
                let response = if split == wire.len() {
                    result.unwrap()
                } else {
                    assert!(result.is_none());
                    assert!(decoder.push(b"").unwrap().is_none());
                    decoder.push(&wire[split..]).unwrap().unwrap()
                };
                assert_eq!(response.body, *expected);
                assert!(decoder.push(b"anything").is_err());
            }
            let mut decoder = ResponseDecoder::new();
            for (index, byte) in wire.iter().enumerate() {
                let response = decoder.push(std::slice::from_ref(byte)).unwrap();
                if index + 1 == wire.len() {
                    assert_eq!(response.unwrap().body, *expected);
                } else {
                    assert!(response.is_none());
                }
            }
        }
    }

    #[test]
    fn malformed_frames_remain_terminal_across_fragments() {
        let cases: &[&[u8]] = &[
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nX: \xff\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFFFFFFFFFF\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx!",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nTrailer: x\r\n\r\n",
        ];
        for wire in cases {
            for size in [1, 7, wire.len()] {
                let mut decoder = ResponseDecoder::new();
                let mut failed = false;
                for chunk in wire.chunks(size) {
                    if decoder.push(chunk).is_err() {
                        failed = true;
                        break;
                    }
                }
                assert!(failed, "invalid frame accepted");
                assert!(decoder.push(b"\r\n0\r\n\r\n").is_err());
            }
        }
    }

    #[test]
    fn header_and_chunk_size_limits_hold_under_fragmentation() {
        let mut header = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nX: ".to_vec();
        header.resize(MAX_HEADER, b'x');
        header.extend_from_slice(b"\r\n\r\n");
        let mut decoder = ResponseDecoder::new();
        for byte in &header[..header.len() - 1] {
            assert!(decoder.push(std::slice::from_ref(byte)).unwrap().is_none());
        }
        assert!(decoder.push(&header[header.len() - 1..]).unwrap().is_some());
        header.insert(MAX_HEADER, b'x');
        assert!(matches!(
            parse_response(&header),
            Err(HttpError::HeaderTooLarge)
        ));

        let prefix = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        for (digits, valid) in [(32, true), (33, false)] {
            let mut wire = prefix.to_vec();
            wire.extend(std::iter::repeat_n(b'0', digits));
            wire.extend_from_slice(b"\r\n\r\n");
            assert_eq!(parse_response(&wire).is_ok(), valid);
        }
    }

    #[test]
    fn body_limit_checks_include_all_chunks_before_their_payloads() {
        let mut decoder = ResponseDecoder::new();
        let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {MAX_BODY}\r\n\r\n");
        assert!(decoder.push(header.as_bytes()).unwrap().is_none());
        assert_eq!(
            decoder
                .push(&vec![b'x'; MAX_BODY])
                .unwrap()
                .unwrap()
                .body
                .len(),
            MAX_BODY
        );

        let prefix = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let mut decoder = ResponseDecoder::new();
        let header = format!("{prefix}{MAX_BODY:x}\r\n");
        assert!(decoder.push(header.as_bytes()).unwrap().is_none());
        assert!(decoder.push(&vec![b'x'; MAX_BODY]).unwrap().is_none());
        assert!(matches!(
            decoder.push(b"\r\n1\r\n"),
            Err(HttpError::BodyTooLarge)
        ));
        let oversized = format!("{prefix}{:x}\r\n", MAX_BODY + 1);
        assert!(matches!(
            parse_response(oversized.as_bytes()),
            Err(HttpError::BodyTooLarge)
        ));
    }
}
