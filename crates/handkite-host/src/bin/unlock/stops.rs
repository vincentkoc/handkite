//! Private receipt IO only; never invoke an unlock, device or mutable gate API.
//!
//! `--stop-state-snapshot > private.json` captures the current disabled state.
//! `--check-stop-state < private.json` checks a previously reviewed receipt.
//! The broker must retain its independent device/service ownership checks and
//! explicit attempt authority. Never automatically replace a mismatched receipt.

use handkite_host::unlock_state::stop_snapshot;
use std::fs::{File, Metadata};
use std::io::{Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

const OUTPUT: &str = "stop_snapshot_output";

fn output_metadata(file: &mut File, size: u64) -> Result<Metadata, &'static str> {
    let metadata = file.metadata().map_err(|_| OUTPUT)?;
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != size
        || flags < 0
        || flags & libc::O_ACCMODE == libc::O_RDONLY
        || file.stream_position().map_err(|_| OUTPUT)? != size
    {
        return Err(OUTPUT);
    }
    handkite_host::release_verify::check_private_input_acl(file).map_err(|_| OUTPUT)?;
    Ok(metadata)
}

fn identity(meta: &Metadata) -> (u64, u64, u32, u32, u32, u64) {
    (
        meta.dev(),
        meta.ino(),
        meta.uid(),
        meta.gid(),
        meta.mode(),
        meta.nlink(),
    )
}

fn write_snapshot(
    output: &mut File,
    capture: impl FnOnce() -> Result<Vec<u8>, &'static str>,
) -> Result<(), &'static str> {
    let before = output_metadata(output, 0)?;
    let receipt = capture()?;
    let ready = output_metadata(output, 0)?;
    if identity(&before) != identity(&ready)
        || (
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec(),
        ) != (
            ready.mtime(),
            ready.mtime_nsec(),
            ready.ctime(),
            ready.ctime_nsec(),
        )
    {
        return Err(OUTPUT);
    }
    output.write_all(&receipt).map_err(|_| OUTPUT)?;
    let after = output_metadata(output, receipt.len() as u64)?;
    if identity(&before) != identity(&after) {
        return Err(OUTPUT);
    }
    Ok(())
}

fn mode(args: &[String]) -> Result<bool, &'static str> {
    match args {
        [_, flag] if flag == "--stop-state-snapshot" => Ok(true),
        [_, flag] if flag == "--check-stop-state" => Ok(false),
        _ => Err("invalid_arguments"),
    }
}

pub(super) fn run(args: &[String]) -> Result<(), String> {
    if mode(args)? {
        let descriptor = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        if descriptor < 0 {
            return Err(OUTPUT.into());
        }
        let mut output = unsafe { File::from_raw_fd(descriptor) };
        write_snapshot(&mut output, stop_snapshot::capture)?;
    } else {
        let descriptor = unsafe { libc::dup(libc::STDIN_FILENO) };
        if descriptor < 0 {
            return Err("stop_snapshot_invalid".into());
        }
        // Own only the duplicate, leaving the process's private input descriptor intact.
        let mut input = unsafe { File::from_raw_fd(descriptor) };
        stop_snapshot::check(&mut input)?;
        println!("stop_snapshot=match");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_modes_accept_no_device_config_or_action_flags() {
        for flag in ["--stop-state-snapshot", "--check-stop-state"] {
            let mut args = vec!["unlock".into(), flag.into()];
            assert_eq!(mode(&args), Ok(flag == "--stop-state-snapshot"));
            for extra in ["--config", "--owned-wda-session", "--state-dir", "--retry"] {
                args.push(extra.into());
                assert_eq!(mode(&args), Err("invalid_arguments"));
                args.pop();
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn snapshot_writes_only_an_empty_private_regular_file() {
        use std::io::{Read, SeekFrom};
        let mut named = tempfile::NamedTempFile::new().unwrap();
        let file = named.as_file_mut();
        write_snapshot(file, || Ok(b"private receipt\n".to_vec())).unwrap();
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"private receipt\n");
        file.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(
            write_snapshot(file, || panic!("captured nonempty output")),
            Err(OUTPUT)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn snapshot_rejects_unsafe_outputs_before_capture() {
        use std::os::unix::fs::PermissionsExt;
        for variant in 0..6 {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            let directory = tempfile::tempdir().unwrap();
            match variant {
                0 => file
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o644))
                    .unwrap(),
                1 => std::fs::hard_link(file.path(), directory.path().join("alias")).unwrap(),
                2 => {
                    file.write_all(b"occupied").unwrap();
                }
                3 => {
                    file.seek(std::io::SeekFrom::Start(1)).unwrap();
                }
                4 => assert!(
                    std::process::Command::new("/bin/chmod")
                        .args(["+a", "everyone allow read"])
                        .arg(file.path())
                        .status()
                        .unwrap()
                        .success()
                ),
                _ => {
                    let mut readonly = File::open(file.path()).unwrap();
                    assert_eq!(
                        write_snapshot(&mut readonly, || panic!("captured read-only output")),
                        Err(OUTPUT)
                    );
                    continue;
                }
            }
            assert_eq!(
                write_snapshot(file.as_file_mut(), || panic!("captured unsafe output")),
                Err(OUTPUT)
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn snapshot_rejects_pipe_and_terminal_before_capture() {
        let mut descriptors = [0; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let _read = unsafe { File::from_raw_fd(descriptors[0]) };
        let mut write = unsafe { File::from_raw_fd(descriptors[1]) };
        assert_eq!(
            write_snapshot(&mut write, || panic!("captured pipe")),
            Err(OUTPUT)
        );
        let (mut master, mut slave) = (0, 0);
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let _master = unsafe { File::from_raw_fd(master) };
        let mut terminal = unsafe { File::from_raw_fd(slave) };
        assert_eq!(
            write_snapshot(&mut terminal, || panic!("captured terminal")),
            Err(OUTPUT)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn snapshot_rechecks_output_after_capture_before_writing() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        assert_eq!(
            write_snapshot(file.as_file_mut(), || {
                std::fs::write(&path, b"other writer").unwrap();
                Ok(b"private receipt".to_vec())
            }),
            Err(OUTPUT)
        );
        assert_eq!(std::fs::read(path).unwrap(), b"other writer");
    }
}
