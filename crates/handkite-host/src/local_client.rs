use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: u64 = 16 * 1024 * 1024;

pub fn exchange(socket: &Path, request: &[u8]) -> io::Result<Vec<u8>> {
    if request.is_empty() || request.len() > MAX_REQUEST || request.contains(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid request frame",
        ));
    }
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(65)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(request)?;
    stream.write_all(b"\n")?;
    let mut response = Vec::new();
    stream.take(MAX_RESPONSE + 2).read_to_end(&mut response)?;
    if response.len() as u64 > MAX_RESPONSE + 1 || response.pop() != Some(b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid response frame",
        ));
    }
    Ok(response)
}
