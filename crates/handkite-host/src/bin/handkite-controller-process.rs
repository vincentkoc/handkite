//! Read-only process comparison with private stdin pins and fixed output only.

use handkite_host::{
    controller_process::{self, Expected, MAX_INPUT_BYTES, Observation},
    release_verify::check_private_input_acl,
};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::process::ExitCode;

fn main() -> ExitCode {
    let observation = run(
        std::env::args_os().skip(1),
        || {
            let fd = io::stdin().as_fd().try_clone_to_owned().ok()?;
            read_input(&mut File::from(fd))
        },
        controller_process::observe,
    );
    let (code, message) = report(observation);
    if io::stdout().lock().write_all(message.as_bytes()).is_err() {
        return ExitCode::from(3);
    }
    ExitCode::from(code)
}

fn run(
    mut args: impl Iterator<Item = OsString>,
    read: impl FnOnce() -> Option<Expected>,
    observe: impl FnOnce(&Expected) -> Observation,
) -> Observation {
    if args.next().as_deref() != Some(OsStr::new("--pinned-stdin")) || args.next().is_some() {
        return Observation::Unknown;
    }
    let Some(expected) = read() else {
        return Observation::Unknown;
    };
    observe(&expected)
}

fn report(observation: Observation) -> (u8, &'static str) {
    match observation {
        Observation::Match => (0, "controller_process=match\n"),
        Observation::Mismatch => (1, "controller_process=mismatch\n"),
        Observation::Absent => (1, "controller_process=absent\n"),
        Observation::Unknown => (3, "controller_process=unknown\n"),
    }
}

fn read_input(file: &mut File) -> Option<Expected> {
    protected_read(file, |file| {
        let mut bytes = Vec::new();
        file.take(MAX_INPUT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        Some(bytes)
    })
}

fn protected_read(
    file: &mut File,
    read: impl FnOnce(&mut File) -> Option<Vec<u8>>,
) -> Option<Expected> {
    let before = file.metadata().ok()?;
    if !before.is_file()
        || before.uid() != unsafe { libc::geteuid() }
        || before.mode() & 0o7777 != 0o600
        || before.nlink() != 1
        || before.len() == 0
        || before.len() > MAX_INPUT_BYTES as u64
        || file.stream_position().ok()? != 0
    {
        return None;
    }
    check_private_input_acl(file).ok()?;
    let bytes = read(file)?;
    if bytes.len() as u64 != before.len() || !same_metadata(&before, &file.metadata().ok()?) {
        return None;
    }
    check_private_input_acl(file).ok()?;
    Expected::parse(&bytes)
}

fn same_metadata(left: &Metadata, right: &Metadata) -> bool {
    (
        left.dev(),
        left.ino(),
        left.uid(),
        left.gid(),
        left.mode(),
        left.nlink(),
        left.len(),
        left.mtime(),
        left.mtime_nsec(),
        left.ctime(),
        left.ctime_nsec(),
    ) == (
        right.dev(),
        right.ino(),
        right.uid(),
        right.gid(),
        right.mode(),
        right.nlink(),
        right.len(),
        right.mtime(),
        right.mtime_nsec(),
        right.ctime(),
        right.ctime_nsec(),
    )
}

#[cfg(test)]
#[path = "controller_process/tests.rs"]
mod tests;
