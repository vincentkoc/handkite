use super::*;
use crate::install_config::RetainedConfig;
use crate::launchd_descriptor::{StagePhase, staging::macos as service};
use handkite_core::protocol::{DeviceBinding, DeviceKey, HardwareId};

const RUNNER_CONFIG: &[u8] = br#"{"device_key":"phone","usb_id":"test-device","wda_runner_bundle_id":"com.example.WDA-Runner.xctrunner"}"#;
const STAGE_TEMP: &str = ".descriptor-33333333333333333333333333333333";

fn fixture() -> Fixture {
    let fixture = Fixture::new();
    fs::write(fixture.layout().config_file(), RUNNER_CONFIG).unwrap();
    fixture
}

fn config(fixture: &Fixture) -> RetainedConfig<'_> {
    RetainedConfig::acquire(
        &fixture.owner,
        &DeviceBinding {
            key: DeviceKey::new("phone").unwrap(),
            usb_id: HardwareId::new("test-device").unwrap(),
        },
    )
    .unwrap()
}

fn hooks<'a>(check: service::Check<'a>) -> service::Hooks<'a> {
    service::Hooks {
        check: Some(check),
        nonce: Some([0x33; 16]),
        ..service::Hooks::production()
    }
}

fn directory(fixture: &Fixture) -> std::path::PathBuf {
    fixture
        .layout()
        .staged_launchd_descriptor()
        .parent()
        .unwrap()
        .into()
}

#[test]
fn stages_exact_fixed_bytes_once_without_changing_protected_state() {
    let fixture = fixture();
    let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
    let mut config = config(&fixture);
    let before = fixture.protected();
    let mut check = |_, _: &Path| Ok(());
    let mut hooks = hooks(&mut check);
    let mut descriptor =
        service::stage(&fixture.owner, &mut config, &mut published, &mut hooks).unwrap();
    assert_eq!(hooks.renames, 1);
    let path = fixture.layout().staged_launchd_descriptor();
    assert_eq!(
        fs::read(&path).unwrap(),
        crate::launchd_descriptor::render(fixture.layout())
            .unwrap()
            .as_bytes()
    );
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o400);
    assert_eq!(fs::read_dir(directory(&fixture)).unwrap().count(), 1);
    service::revalidate(&mut descriptor, &mut |_, _| Ok(())).unwrap();
    fixture.unchanged(before);
    drop(descriptor);
    let error = service::stage(&fixture.owner, &mut config, &mut published, &mut hooks)
        .err()
        .unwrap();
    assert_eq!(error.phase(), StagePhase::BeforeRename);
    assert!(!error.has_residue());
    assert_eq!(hooks.renames, 1);
}

#[test]
fn missing_runner_or_mismatched_lock_rejects_before_creation() {
    let fixture = Fixture::new();
    let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
    let mut config = config(&fixture);
    let mut check = |_, _: &Path| Ok(());
    let error = service::stage(
        &fixture.owner,
        &mut config,
        &mut published,
        &mut hooks(&mut check),
    )
    .err()
    .unwrap();
    assert!(!error.has_residue());
    assert_eq!(fs::read_dir(directory(&fixture)).unwrap().count(), 0);

    let other = self::fixture();
    let mut other_config = self::config(&other);
    let error = service::stage(
        &other.owner,
        &mut other_config,
        &mut published,
        &mut hooks(&mut check),
    )
    .err()
    .unwrap();
    assert!(!error.has_residue());
    assert_eq!(fs::read_dir(directory(&other)).unwrap().count(), 0);
    let mut other_published = run(other.stage(), |_, _| Ok(())).unwrap();
    let error = service::stage(
        &other.owner,
        &mut config,
        &mut other_published,
        &mut hooks(&mut check),
    )
    .err()
    .unwrap();
    assert!(!error.has_residue());
}

#[test]
fn first_stage_refuses_every_existing_entry_and_unsafe_directory() {
    for variant in 0..7 {
        let fixture = fixture();
        let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
        let mut config = config(&fixture);
        let dir = directory(&fixture);
        match variant {
            0 => fs::write(fixture.layout().staged_launchd_descriptor(), b"existing").unwrap(),
            1 => fs::write(dir.join(STAGE_TEMP), b"residue").unwrap(),
            2 => symlink("missing", fixture.layout().staged_launchd_descriptor()).unwrap(),
            3 => fs::create_dir(dir.join("unexpected")).unwrap(),
            4 => chmod(&dir, 0o755),
            5 => acl(&dir),
            _ => {
                let held = dir.with_extension("held");
                fs::rename(&dir, &held).unwrap();
                symlink(held, &dir).unwrap();
            }
        }
        let mut check = |_, _: &Path| Ok(());
        let mut hooks = hooks(&mut check);
        let error = service::stage(&fixture.owner, &mut config, &mut published, &mut hooks)
            .err()
            .unwrap();
        assert_eq!(error.phase(), StagePhase::BeforeRename);
        assert!(!error.has_residue());
        assert_eq!(hooks.renames, 0);
    }
}

#[test]
fn signer_failure_before_creation_before_rename_or_after_rename_preserves_residue() {
    for (fail_at, phase, residue) in [
        (1, StagePhase::BeforeRename, false),
        (4, StagePhase::BeforeRename, true),
        (7, StagePhase::AppliedRename, true),
    ] {
        let fixture = fixture();
        let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
        let mut config = config(&fixture);
        let mut calls = 0;
        let mut check = |_, _: &Path| {
            calls += 1;
            if calls == fail_at {
                Err("synthetic signer failure")
            } else {
                Ok(())
            }
        };
        let error = service::stage(
            &fixture.owner,
            &mut config,
            &mut published,
            &mut hooks(&mut check),
        )
        .err()
        .unwrap();
        assert_eq!(error.phase(), phase);
        assert_eq!(error.has_residue(), residue);
        assert_eq!(
            fs::read_dir(directory(&fixture)).unwrap().count(),
            usize::from(residue)
        );
    }
}

#[test]
fn each_failure_boundary_retains_one_attempt_without_retry_or_cleanup() {
    for point in [
        service::Point::BeforeCreate,
        service::Point::Created,
        service::Point::Written,
        service::Point::Readonly,
        service::Point::Synced,
        service::Point::BeforeRename,
        service::Point::BeforeRenameCall,
        service::Point::Renamed,
        service::Point::BeforeParentSync,
        service::Point::Complete,
    ] {
        let fixture = fixture();
        let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
        let mut config = config(&fixture);
        let mut check = |_, _: &Path| Ok(());
        let mut fault = |at| {
            if at == point {
                Err("synthetic boundary failure")
            } else {
                Ok(())
            }
        };
        let mut hooks = hooks(&mut check);
        hooks.fault = Some(&mut fault);
        let error = service::stage(&fixture.owner, &mut config, &mut published, &mut hooks)
            .err()
            .unwrap();
        let applied = matches!(
            point,
            service::Point::Renamed | service::Point::BeforeParentSync | service::Point::Complete
        );
        assert_eq!(
            error.phase(),
            if applied {
                StagePhase::AppliedRename
            } else {
                StagePhase::BeforeRename
            }
        );
        assert_eq!(hooks.renames, usize::from(applied));
        let residue = point != service::Point::BeforeCreate;
        assert_eq!(error.has_residue(), residue);
        assert_eq!(
            fs::read_dir(directory(&fixture)).unwrap().count(),
            usize::from(residue)
        );
    }
}

#[test]
fn rename_errors_are_classified_without_inferred_recovery() {
    for (code, applied, phase) in [
        (libc::EEXIST, false, StagePhase::BeforeRename),
        (libc::EIO, false, StagePhase::UncertainRename),
        (libc::EIO, true, StagePhase::UncertainRename),
    ] {
        let fixture = fixture();
        let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
        let mut config = config(&fixture);
        let mut check = |_, _: &Path| Ok(());
        let mut hooks = hooks(&mut check);
        hooks.rename_error = Some((code, applied));
        let error = service::stage(&fixture.owner, &mut config, &mut published, &mut hooks)
            .err()
            .unwrap();
        assert_eq!(error.phase(), phase);
        assert!(error.has_residue());
        assert_eq!(hooks.renames, 1);
        assert_eq!(
            fixture.layout().staged_launchd_descriptor().exists(),
            applied
        );
        assert_eq!(directory(&fixture).join(STAGE_TEMP).exists(), !applied);
    }
}

#[test]
fn temporary_file_and_directory_drift_is_never_adopted() {
    for variant in 0..8 {
        let fixture = fixture();
        let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
        let mut config = config(&fixture);
        let temp = directory(&fixture).join(STAGE_TEMP);
        let mut check = |_, _: &Path| Ok(());
        let mut fault = |point| {
            if point == service::Point::BeforeRename {
                match variant {
                    0 => {
                        chmod(&temp, 0o600);
                        fs::write(&temp, b"changed").unwrap();
                        chmod(&temp, 0o400);
                    }
                    1 => {
                        let bytes = fs::read(&temp).unwrap();
                        fs::remove_file(&temp).unwrap();
                        fs::write(&temp, bytes).unwrap();
                        chmod(&temp, 0o400);
                    }
                    2 => fs::hard_link(&temp, directory(&fixture).join("other")).unwrap(),
                    3 => acl(&temp),
                    4 => chmod(&temp, 0o600),
                    5 => fs::write(directory(&fixture).join("other"), b"extra").unwrap(),
                    6 => {
                        let dir = directory(&fixture);
                        fs::rename(&dir, dir.with_extension("held")).unwrap();
                        fs::create_dir(&dir).unwrap();
                        chmod(&dir, 0o700);
                    }
                    _ => {
                        fs::remove_file(&temp).unwrap();
                        symlink("missing", &temp).unwrap();
                    }
                }
            }
            Ok(())
        };
        let mut hooks = hooks(&mut check);
        hooks.fault = Some(&mut fault);
        let error = service::stage(&fixture.owner, &mut config, &mut published, &mut hooks)
            .err()
            .unwrap();
        assert_eq!(error.phase(), StagePhase::BeforeRename);
        assert!(error.has_residue());
        assert_eq!(hooks.renames, 0);
    }
}

#[test]
fn post_rename_drift_rejects_with_applied_classification() {
    for variant in 0..4 {
        let fixture = fixture();
        let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
        let mut config = config(&fixture);
        let mut check = |_, _: &Path| Ok(());
        let mut fault = |point| {
            if point == service::Point::Renamed {
                match variant {
                    0 => fs::write(directory(&fixture).join("other"), b"extra").unwrap(),
                    1 => {
                        let path = fixture.layout().staged_launchd_descriptor();
                        let bytes = fs::read(&path).unwrap();
                        fs::remove_file(&path).unwrap();
                        fs::write(&path, bytes).unwrap();
                        chmod(&path, 0o400);
                    }
                    2 => fs::write(fixture.layout().config_file(), CONFIG).unwrap(),
                    _ => {
                        let path = fixture.directory().join("handkited");
                        chmod(&path, 0o700);
                        fs::write(&path, b"changed").unwrap();
                        chmod(&path, 0o500);
                    }
                }
            }
            Ok(())
        };
        let mut hooks = hooks(&mut check);
        hooks.fault = Some(&mut fault);
        let error = service::stage(&fixture.owner, &mut config, &mut published, &mut hooks)
            .err()
            .unwrap();
        assert_eq!(error.phase(), StagePhase::AppliedRename);
        assert!(error.has_residue());
        assert_eq!(hooks.renames, 1);
    }
}

#[test]
fn retained_handle_rejects_later_replacement_without_resetting_baseline() {
    let fixture = fixture();
    let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
    let mut config = config(&fixture);
    let mut check = |_, _: &Path| Ok(());
    let mut descriptor = service::stage(
        &fixture.owner,
        &mut config,
        &mut published,
        &mut hooks(&mut check),
    )
    .unwrap();
    let path = fixture.layout().staged_launchd_descriptor();
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    fs::write(&path, bytes).unwrap();
    chmod(&path, 0o400);
    assert!(service::revalidate(&mut descriptor, &mut check).is_err());
    assert!(service::revalidate(&mut descriptor, &mut check).is_err());
}

#[test]
fn exclusive_rename_preserves_syscall_time_collisions() {
    for variant in 0..3 {
        let fixture = fixture();
        let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
        let mut config = config(&fixture);
        let path = fixture.layout().staged_launchd_descriptor();
        let mut identity = None;
        let mut check = |_, _: &Path| Ok(());
        let mut fault = |point| {
            if point == service::Point::BeforeRenameCall {
                match variant {
                    0 => fs::write(&path, b"preserved collision").unwrap(),
                    1 => symlink("missing", &path).unwrap(),
                    _ => fs::create_dir(&path).unwrap(),
                }
                identity = Some(Fingerprint::from(fs::symlink_metadata(&path).unwrap()));
            }
            Ok(())
        };
        let mut hooks = hooks(&mut check);
        hooks.fault = Some(&mut fault);
        let error = service::stage(&fixture.owner, &mut config, &mut published, &mut hooks)
            .err()
            .unwrap();
        assert_eq!(error.phase(), StagePhase::BeforeRename);
        assert!(error.has_residue());
        assert_eq!(hooks.renames, 1);
        assert_eq!(
            identity.unwrap(),
            Fingerprint::from(fs::symlink_metadata(&path).unwrap())
        );
        assert!(directory(&fixture).join(STAGE_TEMP).exists());
    }
}

#[test]
fn retained_inputs_drifting_before_rename_prevent_the_syscall() {
    for variant in 0..3 {
        let fixture = fixture();
        let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
        let mut config = config(&fixture);
        let mut check = |_, _: &Path| Ok(());
        let mut fault = |point| {
            if point == service::Point::BeforeRename {
                match variant {
                    0 => fs::write(fixture.layout().config_file(), CONFIG).unwrap(),
                    1 => {
                        let path = fixture.layout().process_lock();
                        fs::remove_file(&path).unwrap();
                        fs::write(&path, b"").unwrap();
                        chmod(&path, 0o600);
                    }
                    _ => {
                        let path = fixture.directory().join("handkited");
                        chmod(&path, 0o700);
                        fs::write(&path, b"changed").unwrap();
                        chmod(&path, 0o500);
                    }
                }
            }
            Ok(())
        };
        let mut hooks = hooks(&mut check);
        hooks.fault = Some(&mut fault);
        let error = service::stage(&fixture.owner, &mut config, &mut published, &mut hooks)
            .err()
            .unwrap();
        assert_eq!(error.phase(), StagePhase::BeforeRename);
        assert!(error.has_residue());
        assert_eq!(hooks.renames, 0);
        assert!(directory(&fixture).join(STAGE_TEMP).exists());
        assert!(!fixture.layout().staged_launchd_descriptor().exists());
    }
}
