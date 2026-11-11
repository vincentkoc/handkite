use super::*;
use std::io::{SeekFrom, Write};
use std::os::fd::OwnedFd;
use std::os::unix::{fs::PermissionsExt, net::UnixStream};
use tempfile::NamedTempFile;

const VALID: &[u8] = br#"{"schema_version":1,"pid":42,"start_seconds":123,"start_microseconds":456,"start_abstime":789,"effective_uid":501,"executable_path":"/test/controller","executable_uuid":"00112233445566778899aabbccddeeff"}"#;

fn fixture(bytes: &[u8]) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file
}

#[test]
fn exact_dispatch_refuses_before_read_or_observation() {
    for args in [vec![], vec!["--pid", "42"], vec!["--pinned-stdin", "extra"]] {
        assert_eq!(
            run(
                args.into_iter().map(Into::into),
                || panic!("must not read"),
                |_| panic!("must not observe")
            ),
            Observation::Unknown
        );
    }
    assert_eq!(
        run(
            ["--pinned-stdin".into()].into_iter(),
            || None,
            |_| panic!("must not observe")
        ),
        Observation::Unknown
    );
    for observation in [
        Observation::Match,
        Observation::Mismatch,
        Observation::Absent,
        Observation::Unknown,
    ] {
        assert_eq!(
            run(
                ["--pinned-stdin".into()].into_iter(),
                || Expected::parse(VALID),
                |_| observation
            ),
            observation
        );
    }
}

#[test]
fn output_is_only_one_fixed_classification_and_exit_code() {
    for (observation, expected) in [
        (Observation::Match, (0, "controller_process=match\n")),
        (Observation::Mismatch, (1, "controller_process=mismatch\n")),
        (Observation::Absent, (1, "controller_process=absent\n")),
        (Observation::Unknown, (3, "controller_process=unknown\n")),
    ] {
        assert_eq!(report(observation), expected);
    }
}

#[test]
fn unsafe_modes_and_sizes_refuse_before_read() {
    for mode in [0o400, 0o640, 0o666, 0o700, 0o4600] {
        let mut file = fixture(VALID);
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .unwrap();
        assert!(protected_read(file.as_file_mut(), |_| panic!("must not read")).is_none());
        assert_eq!(file.stream_position().unwrap(), 0);
    }
    for bytes in [vec![], vec![b' '; MAX_INPUT_BYTES + 1]] {
        let mut file = fixture(&bytes);
        assert!(protected_read(file.as_file_mut(), |_| panic!("must not read")).is_none());
    }
}

#[test]
fn devices_directories_sockets_offsets_and_multiple_links_refuse() {
    let mut device = File::open("/dev/null").unwrap();
    assert!(protected_read(&mut device, |_| panic!("must not read")).is_none());
    let directory = tempfile::tempdir().unwrap();
    assert!(
        protected_read(&mut File::open(directory.path()).unwrap(), |_| panic!(
            "must not read"
        ))
        .is_none()
    );
    let (socket, _peer) = UnixStream::pair().unwrap();
    assert!(
        protected_read(&mut File::from(OwnedFd::from(socket)), |_| panic!(
            "must not read"
        ))
        .is_none()
    );
    let mut file = fixture(VALID);
    file.seek(SeekFrom::Start(1)).unwrap();
    assert!(protected_read(file.as_file_mut(), |_| panic!("must not read")).is_none());
    file.seek(SeekFrom::Start(0)).unwrap();
    std::fs::hard_link(file.path(), directory.path().join("linked")).unwrap();
    assert!(protected_read(file.as_file_mut(), |_| panic!("must not read")).is_none());
}

#[cfg(target_os = "macos")]
#[test]
fn protected_file_admits_only_stable_complete_valid_contents() {
    assert!(read_input(fixture(VALID).as_file_mut()).is_some());
    assert!(read_input(fixture(b"{}").as_file_mut()).is_none());
    assert!(
        protected_read(fixture(VALID).as_file_mut(), |_| Some(
            VALID[..VALID.len() - 1].to_vec()
        ))
        .is_none()
    );
    assert!(protected_read(fixture(VALID).as_file_mut(), |_| None).is_none());
    assert!(
        protected_read(fixture(VALID).as_file_mut(), |file| {
            file.set_permissions(std::fs::Permissions::from_mode(0o640))
                .unwrap();
            Some(VALID.to_vec())
        })
        .is_none()
    );
    assert!(
        protected_read(fixture(VALID).as_file_mut(), |file| {
            file.set_len(VALID.len() as u64 + 1).unwrap();
            Some(VALID.to_vec())
        })
        .is_none()
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn unsupported_acl_platform_refuses_before_read() {
    assert!(protected_read(fixture(VALID).as_file_mut(), |_| panic!("must not read")).is_none());
}
