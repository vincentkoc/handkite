use crate::wire::{Reply, Request, dispatch};
use handkite_core::controller::{Controller, DeviceAdapter};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub fn bind_protected(directory: &Path) -> io::Result<UnixListener> {
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir()
        || metadata.permissions().mode() & 0o777 != 0o700
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket directory must be owned by this user and mode 0700",
        ));
    }
    let socket = directory.join("handkite.sock");
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub fn serve<A: DeviceAdapter>(
    listener: UnixListener,
    controller: &Controller<A>,
) -> io::Result<()> {
    for accepted in listener.incoming() {
        let mut stream = accepted?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(15)))?;
        let _ = handle(&mut stream, controller);
    }
    Ok(())
}

pub(crate) fn handle<A: DeviceAdapter>(
    stream: &mut UnixStream,
    controller: &Controller<A>,
) -> io::Result<()> {
    let reply = match read_request(stream) {
        Ok(bytes) => match serde_json::from_slice::<Request>(&bytes) {
            Ok(request) => dispatch(controller, request),
            Err(_) => Reply::Error {
                code: "InvalidRequest".into(),
            },
        },
        Err(_) => Reply::Error {
            code: "InvalidRequest".into(),
        },
    };
    let bytes = serde_json::to_vec(&reply).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE {
        return write_reply(stream, br#"{"type":"error","code":"ResponseTooLarge"}"#);
    }
    write_reply(stream, &bytes)
}

fn write_reply(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    stream.write_all(bytes)?;
    stream.write_all(b"\n")
}

fn read_request(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut input = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "missing newline",
            ));
        }
        if let Some(end) = chunk[..read].iter().position(|byte| *byte == b'\n') {
            if input.len() + end > MAX_REQUEST || end + 1 != read {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid frame"));
            }
            input.extend_from_slice(&chunk[..end]);
            return Ok(input);
        }
        if input.len() + read > MAX_REQUEST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        input.extend_from_slice(&chunk[..read]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    #[test]
    fn socket_has_private_permissions_and_refuses_a_second_owner() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("handkite-test-{}-{unique}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let listener = bind_protected(&directory).unwrap();
        let socket = directory.join("handkite.sock");
        assert_eq!(
            fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(bind_protected(&directory).is_err());
        drop(listener);
        fs::remove_file(socket).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
