use crate::service_lock::ServiceLock;
use crate::wire::{Reply, Request, dispatch};
use handkite_core::controller::{Controller, DeviceAdapter};
use std::fs::{self, Metadata};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_REQUEST: usize = 64 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const REQUEST_BUDGET: Duration = Duration::from_secs(10);
const READ_POLL: Duration = Duration::from_millis(100);
const RESPONSE_BUDGET: Duration = Duration::from_secs(15);
const WRITE_POLL: Duration = Duration::from_millis(100);

/// The listener cannot outlive the process lock that authorized its creation.
pub struct ServiceListener<'a> {
    listener: UnixListener,
    _owner: &'a ServiceLock,
}

impl ServiceListener<'_> {
    pub fn serve<A: DeviceAdapter>(self, controller: &Controller<A>) -> io::Result<()> {
        serve(self.listener, controller)
    }

    /// Stop accepting new work after the daemon observes a shutdown signal.
    pub fn serve_until<A: DeviceAdapter>(
        self,
        controller: &Controller<A>,
        should_stop: impl Fn() -> bool,
    ) -> io::Result<()> {
        serve_until(self.listener, controller, should_stop)
    }
}

pub fn bind_service(owner: &ServiceLock) -> io::Result<ServiceListener<'_>> {
    owner.validate_current()?;
    let socket = owner.layout().socket();
    match fs::symlink_metadata(&socket) {
        Ok(original) => {
            validate_socket(&original, unsafe { libc::geteuid() })?;
            require_no_listener(&socket)?;
            remove_unchanged_socket(owner, &original)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    owner.validate_current()?;
    let listener = bind_protected(&owner.layout().runtime_directory())?;
    owner.validate_current()?;
    validate_socket(&fs::symlink_metadata(socket)?, unsafe { libc::geteuid() })?;
    Ok(ServiceListener {
        listener,
        _owner: owner,
    })
}

fn validate_socket(metadata: &Metadata, uid: u32) -> io::Result<()> {
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe service socket",
        ));
    }
    Ok(())
}

fn socket_fingerprint(metadata: &Metadata) -> impl PartialEq + use<> {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.uid(),
        metadata.gid(),
        metadata.nlink(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

fn remove_unchanged_socket(owner: &ServiceLock, original: &Metadata) -> io::Result<()> {
    owner.validate_current()?;
    let current = fs::symlink_metadata(owner.layout().socket())?;
    validate_socket(&current, unsafe { libc::geteuid() })?;
    if socket_fingerprint(original) != socket_fingerprint(&current) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "service socket changed",
        ));
    }
    // Removal remains anchored to the locked runtime even if its path is renamed.
    if unsafe {
        libc::unlinkat(
            owner.runtime_directory().as_raw_fd(),
            c"handkite.sock".as_ptr(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn require_no_listener(path: &Path) -> io::Result<()> {
    // A stream connect can report ECONNREFUSED for a full live backlog on Darwin.
    // Datagram connect checks endpoint existence/type without entering that backlog.
    // See XNU bsd/kern/uipc_usrreq.c, unp_connect: vnode, type, then accept queue.
    let probe = UnixDatagram::unbound()?;
    probe.set_nonblocking(true)?;
    match probe.connect(path) {
        Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "service socket may have a live listener",
        )),
    }
}

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

#[cfg(test)]
#[path = "local_socket/service_tests.rs"]
mod service_tests;

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

pub fn serve_until<A: DeviceAdapter>(
    listener: UnixListener,
    controller: &Controller<A>,
    should_stop: impl Fn() -> bool,
) -> io::Result<()> {
    accept_until(listener, &should_stop, |mut stream| {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(15)))?;
        let _ = handle_checked(&mut stream, controller, &should_stop);
        Ok(())
    })
}

fn accept_until(
    listener: UnixListener,
    should_stop: &impl Fn() -> bool,
    mut serve_stream: impl FnMut(UnixStream) -> io::Result<()>,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    while !should_stop() {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                if should_stop() {
                    break;
                }
                serve_stream(stream)?;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn handle<A: DeviceAdapter>(
    stream: &mut UnixStream,
    controller: &Controller<A>,
) -> io::Result<()> {
    handle_checked(stream, controller, &|| false)
}

fn handle_checked<A: DeviceAdapter>(
    stream: &mut UnixStream,
    controller: &Controller<A>,
    should_stop: &impl Fn() -> bool,
) -> io::Result<()> {
    let request = read_request_until(stream, should_stop, Instant::now() + REQUEST_BUDGET);
    if should_stop() {
        return Ok(());
    }
    let reply = match request {
        Ok(bytes) => match serde_json::from_slice::<Request>(&bytes) {
            Ok(request) if !should_stop() => dispatch(controller, request),
            Ok(_) => return Ok(()),
            Err(_) => Reply::Error {
                code: "InvalidRequest".into(),
            },
        },
        Err(_) => Reply::Error {
            code: "InvalidRequest".into(),
        },
    };
    let bytes = serde_json::to_vec(&reply).map_err(io::Error::other)?;
    let deadline = Instant::now() + RESPONSE_BUDGET;
    if bytes.len() > MAX_RESPONSE {
        return write_reply_until(
            stream,
            br#"{"type":"error","code":"ResponseTooLarge"}"#,
            should_stop,
            deadline,
        );
    }
    write_reply_until(stream, &bytes, should_stop, deadline)
}

fn write_reply_until(
    stream: &mut UnixStream,
    bytes: &[u8],
    should_stop: &impl Fn() -> bool,
    deadline: Instant,
) -> io::Result<()> {
    for chunk in [bytes, b"\n"] {
        let mut sent = 0;
        while sent < chunk.len() {
            if should_stop() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "service stopping",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "response timed out",
                ));
            }
            stream.set_write_timeout(Some(remaining.min(WRITE_POLL)))?;
            match stream.write(&chunk[sent..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "response stalled")),
                Ok(written) => sent += written,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn read_request_until(
    stream: &mut UnixStream,
    should_stop: &impl Fn() -> bool,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let mut input = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 4096];
    loop {
        if should_stop() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "service stopping",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request timed out"));
        }
        stream.set_read_timeout(Some(remaining.min(READ_POLL)))?;
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
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

    fn constrained_pair() -> (UnixStream, UnixStream) {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let size: libc::c_int = 1024;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    sender.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        (sender, receiver)
    }

    #[test]
    fn slow_reader_cannot_extend_response_deadline() {
        let (mut sender, _receiver) = constrained_pair();
        let started = Instant::now();
        let result = write_reply_until(
            &mut sender,
            &vec![b'x'; 1024 * 1024],
            &|| false,
            started + Duration::from_millis(80),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn shutdown_interrupts_a_blocked_response() {
        let (mut sender, _receiver) = constrained_pair();
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = stopped.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            signal.store(true, std::sync::atomic::Ordering::Release);
        });
        let started = Instant::now();
        let result = write_reply_until(
            &mut sender,
            &vec![b'x'; 1024 * 1024],
            &|| stopped.load(std::sync::atomic::Ordering::Acquire),
            started + Duration::from_secs(1),
        );
        setter.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn idle_listener_stops_without_a_client() {
        let directory = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(directory.path().join("socket")).unwrap();
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = stopped.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            signal.store(true, std::sync::atomic::Ordering::Release);
        });
        let started = std::time::Instant::now();
        accept_until(
            listener,
            &|| stopped.load(std::sync::atomic::Ordering::Acquire),
            |_| panic!("no client was admitted"),
        )
        .unwrap();
        setter.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stop_after_accept_discards_queued_request() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        let _queued = UnixStream::connect(socket).unwrap();
        let checks = std::cell::Cell::new(0);
        accept_until(
            listener,
            &|| {
                checks.set(checks.get() + 1);
                checks.get() == 2
            },
            |_| panic!("queued client must not dispatch after stop"),
        )
        .unwrap();
        assert_eq!(checks.get(), 2);
    }

    #[test]
    fn partial_request_stops_promptly() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(b"{").unwrap();
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = stopped.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            signal.store(true, std::sync::atomic::Ordering::Release);
        });
        let started = Instant::now();
        let result = read_request_until(
            &mut server,
            &|| stopped.load(std::sync::atomic::Ordering::Acquire),
            started + Duration::from_secs(1),
        );
        setter.join().unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn trickling_client_cannot_extend_request_deadline() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let sender = std::thread::spawn(move || {
            for _ in 0..6 {
                std::thread::sleep(Duration::from_millis(20));
                if client.write_all(b"x").is_err() {
                    break;
                }
            }
        });
        let started = Instant::now();
        let result =
            read_request_until(&mut server, &|| false, started + Duration::from_millis(75));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(300));
        sender.join().unwrap();
    }

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
