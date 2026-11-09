use super::*;
use crate::service_layout::tests::socket_fixture;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};

#[test]
fn crashed_owner_can_restart_without_changing_pin_marker() {
    const CHILD: &str = "HANDKITE_TEST_SOCKET_RUNTIME";
    if let Some(runtime) = std::env::var_os(CHILD) {
        let runtime = Path::new(&runtime);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(runtime.join("handkite.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let _listener = bind_protected(runtime).unwrap();
        println!("owned");
        std::io::stdout().flush().unwrap();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        return;
    }
    let (_scratch, layout) = socket_fixture();
    let marker_directory = layout.state_directory().join("iphone");
    fs::create_dir(&marker_directory).unwrap();
    let marker = marker_directory.join("UNLOCK-DISABLED");
    fs::write(&marker, b"existing-stop\n").unwrap();
    let original_marker = fs::metadata(&marker).unwrap();
    drop(ServiceLock::acquire(layout.clone()).unwrap());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "local_socket::service_tests::crashed_owner_can_restart_without_changing_pin_marker",
            "--nocapture",
        ])
        .env(CHILD, layout.runtime_directory())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if line.trim() == "owned" {
            break;
        }
        line.clear();
    }
    let refused = ServiceLock::acquire(layout.clone()).is_err();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(refused);
    assert!(
        fs::symlink_metadata(layout.socket())
            .unwrap()
            .file_type()
            .is_socket()
    );
    let owner = ServiceLock::acquire(layout.clone()).unwrap();
    let listener = bind_service(&owner).unwrap();
    let stream = UnixStream::connect(layout.socket()).unwrap();
    drop(stream);
    assert_eq!(
        fs::metadata(layout.socket()).unwrap().mode() & 0o7777,
        0o600
    );
    drop(listener);
    // Normal process shutdown also leaves a reclaimable socket.
    drop(bind_service(&owner).unwrap());
    assert_eq!(fs::read(&marker).unwrap(), b"existing-stop\n");
    assert!(
        socket_fingerprint(&original_marker) == socket_fingerprint(&fs::metadata(marker).unwrap())
    );
}

#[test]
fn live_listener_is_preserved_and_probe_sends_no_request() {
    let (_scratch, layout) = socket_fixture();
    let owner = ServiceLock::acquire(layout.clone()).unwrap();
    let listener = bind_protected(&layout.runtime_directory()).unwrap();
    let original = fs::metadata(layout.socket()).unwrap();
    assert!(bind_service(&owner).is_err());
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(
        socket_fingerprint(&original)
            == socket_fingerprint(&fs::metadata(layout.socket()).unwrap())
    );
}

#[test]
fn unsafe_socket_entries_are_never_removed() {
    let (_scratch, layout) = socket_fixture();
    let owner = ServiceLock::acquire(layout.clone()).unwrap();
    let socket = layout.socket();
    let target = layout.runtime_directory().join("keep");
    fs::write(&target, b"keep\n").unwrap();
    symlink(&target, &socket).unwrap();
    assert!(bind_service(&owner).is_err());
    assert!(
        fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_file(&socket).unwrap();
    fs::write(&socket, b"keep socket entry\n").unwrap();
    assert!(bind_service(&owner).is_err());
    assert_eq!(fs::read(&socket).unwrap(), b"keep socket entry\n");
    fs::remove_file(&socket).unwrap();
    fs::create_dir(&socket).unwrap();
    assert!(bind_service(&owner).is_err());
    assert!(socket.is_dir());
    fs::remove_dir(&socket).unwrap();
    drop(bind_protected(&layout.runtime_directory()).unwrap());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(bind_service(&owner).is_err());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        validate_socket(
            &fs::metadata(&socket).unwrap(),
            unsafe { libc::geteuid() }.wrapping_add(1)
        )
        .is_err()
    );
    let alias = layout.runtime_directory().join("alias.sock");
    fs::hard_link(&socket, &alias).unwrap();
    assert!(bind_service(&owner).is_err());
    assert!(socket.exists());
    assert_eq!(fs::read(&target).unwrap(), b"keep\n");
}

#[test]
fn socket_replacement_or_metadata_change_after_probe_is_preserved() {
    let (_scratch, layout) = socket_fixture();
    let owner = ServiceLock::acquire(layout.clone()).unwrap();
    let socket = layout.socket();
    drop(bind_protected(&layout.runtime_directory()).unwrap());
    let original = fs::metadata(&socket).unwrap();
    require_no_listener(&socket).unwrap();
    let held = socket.with_extension("held");
    fs::rename(&socket, &held).unwrap();
    let live_replacement = bind_protected(&layout.runtime_directory()).unwrap();
    assert!(remove_unchanged_socket(&owner, &original).is_err());
    let connection = UnixStream::connect(&socket).unwrap();
    let (accepted, _) = live_replacement.accept().unwrap();
    drop(connection);
    drop(accepted);
    drop(live_replacement);
    // Use a fresh stale fixture: Darwin can retain a recently connected endpoint briefly.
    fs::remove_file(&socket).unwrap();
    drop(bind_protected(&layout.runtime_directory()).unwrap());
    let original = fs::metadata(&socket).unwrap();
    require_no_listener(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(remove_unchanged_socket(&owner, &original).is_err());
    assert!(socket.exists());
    assert!(held.exists());
}

#[test]
fn full_listener_backlog_fails_closed_without_blocking() {
    let (_scratch, layout) = socket_fixture();
    let owner = ServiceLock::acquire(layout.clone()).unwrap();
    let listener = bind_protected(&layout.runtime_directory()).unwrap();
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
    let original = fs::metadata(layout.socket()).unwrap();
    let mut queued = Vec::new();
    let mut saturated = false;
    for _ in 0..16 {
        let stream = nonblocking_stream_connect(&layout.socket());
        if stream.is_err() {
            saturated = true;
            break;
        }
        queued.push(stream.unwrap());
    }
    assert!(!queued.is_empty());
    assert!(saturated);
    for _ in 0..16 {
        assert!(bind_service(&owner).is_err());
    }
    assert!(
        socket_fingerprint(&original)
            == socket_fingerprint(&fs::metadata(layout.socket()).unwrap())
    );
}

#[test]
fn lost_lock_refuses_reclamation_and_absence_is_not_stale_proof() {
    let (_scratch, layout) = socket_fixture();
    let owner = ServiceLock::acquire(layout.clone()).unwrap();
    assert!(require_no_listener(&layout.socket()).is_err());
    drop(bind_protected(&layout.runtime_directory()).unwrap());
    let original = fs::metadata(layout.socket()).unwrap();
    fs::rename(
        layout.process_lock(),
        layout.process_lock().with_extension("held"),
    )
    .unwrap();
    assert!(bind_service(&owner).is_err());
    assert!(
        socket_fingerprint(&original)
            == socket_fingerprint(&fs::metadata(layout.socket()).unwrap())
    );
}

// Fill a live accept queue without risking a blocking connect on any Unix host.
fn nonblocking_stream_connect(path: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    assert!(bytes.len() < address.sun_path.len());
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as u8;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0);
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    assert_ne!(
        unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
        -1
    );
    assert_ne!(
        unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        -1
    );
    let result = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}
