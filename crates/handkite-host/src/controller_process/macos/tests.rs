use super::*;
use crate::controller_process::{Expected, Observation, observe};

fn basic_fixture() -> Basic {
    Basic {
        start_seconds: 123,
        start_microseconds: 456,
        effective_uid: 501,
    }
}

fn executable_fixture() -> Executable {
    Executable {
        uuid: [1; 16],
        start_abstime: 789,
    }
}

#[test]
fn within_sample_identity_changes_and_late_disappearance_are_unknown() {
    for change_uid in [false, true] {
        let mut calls = 0;
        let result = sample_with(
            || {
                calls += 1;
                let mut basic = basic_fixture();
                if calls == 2 {
                    if change_uid {
                        basic.effective_uid += 1;
                    } else {
                        basic.start_seconds += 1;
                    }
                }
                Ok(basic)
            },
            || Ok(executable_fixture()),
            || Ok("/test/controller".into()),
        );
        assert!(matches!(result, Err(ReadFailure::Unknown)));
    }
    for change_uuid in [false, true] {
        let mut calls = 0;
        let result = sample_with(
            || Ok(basic_fixture()),
            || {
                calls += 1;
                let mut executable = executable_fixture();
                if calls == 2 {
                    if change_uuid {
                        executable.uuid[0] += 1;
                    } else {
                        executable.start_abstime += 1;
                    }
                }
                Ok(executable)
            },
            || Ok("/test/controller".into()),
        );
        assert!(matches!(result, Err(ReadFailure::Unknown)));
    }
    let mut calls = 0;
    let result = sample_with(
        || {
            calls += 1;
            if calls == 1 {
                Ok(basic_fixture())
            } else {
                Err(ReadFailure::Absent)
            }
        },
        || Ok(executable_fixture()),
        || Ok("/test/controller".into()),
    );
    assert!(matches!(result, Err(ReadFailure::Unknown)));
    let result = sample_with(
        || Ok(basic_fixture()),
        || Err(ReadFailure::Absent),
        || panic!("must stop"),
    );
    assert!(matches!(result, Err(ReadFailure::Unknown)));
    let result = sample_with(
        || Ok(basic_fixture()),
        || Ok(executable_fixture()),
        || Err(ReadFailure::Absent),
    );
    assert!(matches!(result, Err(ReadFailure::Unknown)));
}

#[test]
fn only_exact_bsd_count_or_zero_with_esrch_is_admitted() {
    assert_eq!(check_bsd_count(136, Some(libc::ESRCH)), Ok(()));
    assert_eq!(
        check_bsd_count(0, Some(libc::ESRCH)),
        Err(ReadFailure::Absent)
    );
    for count in [-1, 1, 135, 137] {
        assert_eq!(
            check_bsd_count(count, Some(libc::ESRCH)),
            Err(ReadFailure::Unknown)
        );
    }
    for errno in [
        None,
        Some(0),
        Some(libc::EPERM),
        Some(libc::EACCES),
        Some(libc::EINVAL),
    ] {
        assert_eq!(check_bsd_count(0, errno), Err(ReadFailure::Unknown));
    }
}

#[test]
fn path_requires_exact_nonempty_terminated_utf8_without_normalization() {
    assert_eq!(
        decode_path(b"/test/tool\0", 10).as_deref(),
        Some("/test/tool")
    );
    for (bytes, count) in [
        (b"/test/tool\0".as_slice(), 0),
        (b"/test/tool\0", -1),
        (b"/test/tool\0", 11),
        (b"/test/tool\0", 9),
        (b"/test/toolx", 10),
        (b"/test\0tool\0", 10),
        (b"/bad\xff\0", 5),
        (b"/test/../tool\0", 13),
    ] {
        assert!(decode_path(bytes, count).is_none());
    }
}

#[test]
fn native_current_test_process_matches_its_executable_metadata() {
    let pid = std::process::id() as i32;
    let identity = sample(pid).unwrap_or_else(|_| panic!("test process sample unavailable"));
    assert_eq!(identity.effective_uid, unsafe { libc::geteuid() });
    assert_eq!(
        std::path::Path::new(&identity.executable_path),
        std::env::current_exe().unwrap()
    );
    // Independently read LC_UUID from this test executable only. This checks
    // kernel UUID semantics, without treating the UUID as signature evidence.
    let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    assert_eq!(&bytes[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    let word = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    let mut offset = 32;
    let mut uuid = None;
    for _ in 0..word(16) {
        let size = word(offset + 4) as usize;
        assert!(size >= 8);
        if word(offset) == 0x1b {
            uuid = Some(&bytes[offset + 8..offset + 24]);
        }
        offset += size;
    }
    assert_eq!(uuid, Some(identity.executable_uuid.as_slice()));
    let mut expected = Expected { pid, identity };
    assert_eq!(observe(&expected), Observation::Match);
    expected.identity.start_abstime += 1;
    assert_eq!(observe(&expected), Observation::Mismatch);
}

#[test]
fn native_reaped_test_child_is_absent_at_observation() {
    let mut child = std::process::Command::new("/usr/bin/true")
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    assert!(child.wait().unwrap().success());
    let expected = Expected {
        pid,
        identity: sample(std::process::id() as i32)
            .unwrap_or_else(|_| panic!("test process sample unavailable")),
    };
    assert_eq!(observe(&expected), Observation::Absent);
}
