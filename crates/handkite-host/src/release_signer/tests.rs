use super::*;
use std::ffi::OsStr;
use std::os::unix::process::ExitStatusExt;

const CERTIFICATE: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn requires_exact_lowercase_certificate_hash() {
    for certificate in [
        "".to_owned(),
        "-".to_owned(),
        CERTIFICATE.to_uppercase(),
        format!(" {CERTIFICATE}"),
        format!("{CERTIFICATE}\n"),
        "a".repeat(39),
        "a".repeat(41),
        "g".repeat(40),
        format!("{}\" or always", "a".repeat(40)),
    ] {
        assert!(matches!(
            SignerPolicy::new(&certificate, ArtifactName::Daemon),
            Err(INVALID_CERTIFICATE)
        ));
    }
    assert!(SignerPolicy::new(CERTIFICATE, ArtifactName::Daemon).is_ok());
}

#[test]
fn passes_exact_requirement_and_single_literal_path_to_fixed_tool() {
    for artifact in [ArtifactName::Daemon, ArtifactName::Cli, ArtifactName::Mcp] {
        let policy = SignerPolicy::new(CERTIFICATE, artifact).unwrap();
        let path = Path::new("/candidate/123 -R=always; $(false)");
        assert_eq!(
            policy.check_with(path, |command, timeout| {
                assert_eq!(command.get_program(), OsStr::new("/usr/bin/codesign"));
                let requirement = format!(
                    "=anchor apple generic and certificate leaf = H\"{CERTIFICATE}\" and identifier \"{}\"",
                    artifact.code_identifier()
                );
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    [
                        OsStr::new("--verify"),
                        OsStr::new("--strict=all"),
                        OsStr::new("--all-architectures"),
                        OsStr::new("--test-requirement"),
                        OsStr::new(&requirement),
                        OsStr::new("--"),
                        path.as_os_str(),
                    ]
                );
                assert_eq!(timeout, Duration::from_secs(10));
                Ok(ExitStatus::from_raw(0))
            }),
            Ok(())
        );
    }
}

#[test]
fn rejects_process_ids_and_relative_paths_before_running_tool() {
    let policy = SignerPolicy::new(CERTIFICATE, ArtifactName::Daemon).unwrap();
    for path in ["", "123", "+123", "--verify", "./candidate", "candidate"] {
        assert_eq!(
            policy.check_with(Path::new(path), |_, _| panic!("tool must not run")),
            Err("signature check requires an absolute candidate path")
        );
    }
}

#[test]
fn maps_all_tool_failures_to_fixed_errors() {
    let policy = SignerPolicy::new(CERTIFICATE, ArtifactName::Daemon).unwrap();
    let path = Path::new("/candidate/private-name");
    // codesign uses 1 for invalid code, 2 for tool errors, 3 for unmet policy.
    for status in [1 << 8, 2 << 8, 3 << 8, 127 << 8, libc::SIGKILL] {
        assert_eq!(
            policy.check_with(path, |_, _| Ok(ExitStatus::from_raw(status))),
            Err(REJECTED)
        );
    }
    for (failure, message) in [
        (RunError::Unavailable, "signature check unavailable"),
        (RunError::Timeout, "signature check timed out"),
    ] {
        assert_eq!(policy.check_with(path, |_, _| Err(failure)), Err(message));
    }
}

#[test]
fn runner_rejects_spawn_failure() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        run_command(
            &mut Command::new(directory.path().join("absent")),
            CHECK_TIMEOUT
        ),
        Err(RunError::Unavailable)
    );
}

#[test]
fn runner_bounds_noisy_child_and_reaps_it() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "release_signer::tests::noisy_child",
            "--nocapture",
        ])
        .env("HANDKITE_SIGNER_TEST_CHILD", "1");
    let start = Instant::now();
    assert_eq!(
        run_command(&mut command, Duration::from_millis(100)),
        Err(RunError::Timeout)
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn noisy_child() {
    if std::env::var_os("HANDKITE_SIGNER_TEST_CHILD").is_none() {
        return;
    }
    let output = "fixture diagnostics".repeat(1024);
    loop {
        println!("{output}");
        eprintln!("{output}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn native_unsigned_adhoc_wrong_identifier_and_tampered_code_fail() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("candidate");
    std::fs::copy("/usr/bin/true", &path).unwrap();
    let policy = SignerPolicy::new(CERTIFICATE, ArtifactName::Daemon).unwrap();
    let native = |args: &[&str]| {
        run_command(
            Command::new("/usr/bin/codesign")
                .env_clear()
                .args(args)
                .arg("--")
                .arg(&path),
            CHECK_TIMEOUT,
        )
        .unwrap()
    };
    assert!(native(&["--remove-signature"]).success());
    assert!(!native(&["--verify", "--strict=all", "--all-architectures"]).success());
    assert_eq!(policy.check(&path), Err(REJECTED));

    for identifier in ["com.handkite.handkited", "com.example.wrong"] {
        assert!(
            native(&[
                "--force",
                "--sign",
                "-",
                "--identifier",
                identifier,
                "--timestamp=none",
            ])
            .success()
        );
        assert!(native(&["--verify", "--strict=all", "--all-architectures"]).success());
        // Exit 3 proves the literal requirement parsed and rejected valid ad-hoc
        // code, rather than merely failing because of malformed CLI arguments.
        assert_eq!(
            policy.check_with(&path, |command, timeout| {
                let status = run_command(command, timeout)?;
                assert_eq!(status.code(), Some(3));
                Ok(status)
            }),
            Err(REJECTED)
        );
    }
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(!native(&["--verify", "--strict=all", "--all-architectures"]).success());
    assert_eq!(policy.check(&path), Err(REJECTED));
}
