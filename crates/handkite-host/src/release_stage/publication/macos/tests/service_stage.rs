use super::*;
use crate::install_config::RetainedConfig;
use crate::launchd_descriptor::{StagePhase, staging::macos as service};
use handkite_core::protocol::{DeviceBinding, DeviceKey, HardwareId};

mod registration;

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

fn staged_inputs(fixture: &Fixture) -> (RetainedConfig<'_>, PublishedRelease<'_>) {
    let mut published = run(fixture.stage(), |_, _| Ok(())).unwrap();
    let mut config = config(fixture);
    drop(
        service::stage(
            &fixture.owner,
            &mut config,
            &mut published,
            &mut hooks(&mut |_, _| Ok(())),
        )
        .unwrap(),
    );
    (config, published)
}

fn descriptor_fingerprints(fixture: &Fixture) -> Vec<Option<Fingerprint>> {
    let mut paths = vec![
        directory(fixture),
        fixture.layout().staged_launchd_descriptor(),
        fixture.layout().process_lock(),
        fixture.layout().releases_directory(),
        fixture.directory(),
        fixture.receipt(),
    ];
    paths.extend(NAMES.map(|name| fixture.directory().join(name.filename())));
    paths
        .into_iter()
        .map(|path| fs::symlink_metadata(path).ok().map(Fingerprint::from))
        .collect()
}

fn replace_descriptor(fixture: &Fixture, parent: bool) {
    let path = fixture.layout().staged_launchd_descriptor();
    let bytes = fs::read(&path).unwrap();
    if parent {
        let dir = directory(fixture);
        fs::rename(&dir, dir.with_extension("held")).unwrap();
        fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o700);
    } else {
        fs::rename(&path, fixture._home.path().join("held-descriptor")).unwrap();
    }
    fs::write(&path, bytes).unwrap();
    chmod(&path, 0o400);
}

#[test]
fn reopening_after_drop_preserves_exact_file_and_protected_inputs() {
    let fixture = fixture();
    let (mut config, mut published) = staged_inputs(&fixture);
    let before = descriptor_fingerprints(&fixture);
    let protected = fixture.protected();
    let bytes = fs::read(fixture.layout().staged_launchd_descriptor()).unwrap();
    let mut seen = Vec::new();
    let mut descriptor = service::reopen_staged(
        &fixture.owner,
        &mut config,
        &mut published,
        Some(&mut |name, path| {
            seen.push((name, path.to_owned()));
            Ok(())
        }),
    )
    .unwrap();
    let expected = NAMES.map(|name| (name, fixture.directory().join(name.filename())));
    assert_eq!(seen, [expected.clone(), expected].concat());
    service::revalidate(&mut descriptor, &mut |_, _| Ok(())).unwrap();
    service::revalidate(&mut descriptor, &mut |_, _| Ok(())).unwrap();
    drop(descriptor);
    assert_eq!(before, descriptor_fingerprints(&fixture));
    fixture.unchanged(protected);
    assert_eq!(
        bytes,
        fs::read(fixture.layout().staged_launchd_descriptor()).unwrap()
    );
    let mut check = |_, _: &Path| Ok(());
    let mut hooks = hooks(&mut check);
    let error = service::stage(&fixture.owner, &mut config, &mut published, &mut hooks)
        .err()
        .unwrap();
    assert_eq!(error.phase(), StagePhase::BeforeRename);
    assert!(!error.has_residue());
    assert_eq!(hooks.renames, 0);
    assert_eq!(before, descriptor_fingerprints(&fixture));
}

fn change_descriptor(fixture: &Fixture, variant: usize) {
    let path = fixture.layout().staged_launchd_descriptor();
    let dir = directory(fixture);
    match variant {
        0 => fs::remove_file(&path).unwrap(),
        1 => fs::write(dir.join("extra"), b"preserved").unwrap(),
        2 => {
            fs::remove_file(&path).unwrap();
            symlink("missing", &path).unwrap();
        }
        3 => fs::hard_link(&path, fixture._home.path().join("linked-descriptor")).unwrap(),
        4 => chmod(&path, 0o600),
        5 => acl(&path),
        6 => {
            let mut bytes = fs::read(&path).unwrap();
            bytes[0] = b'!';
            chmod(&path, 0o600);
            fs::write(&path, bytes).unwrap();
            chmod(&path, 0o400);
        }
        7 => chmod(&dir, 0o755),
        8 => acl(&dir),
        9 => {
            let held = dir.with_extension("held");
            fs::rename(&dir, &held).unwrap();
            symlink(held, &dir).unwrap();
        }
        10 => fs::rename(&dir, dir.with_extension("held")).unwrap(),
        _ => {
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            chmod(&path, 0o400);
        }
    }
}

#[test]
fn reopening_refuses_missing_extra_linked_unsafe_or_inexact_descriptor() {
    for variant in 0..12 {
        let fixture = fixture();
        let (mut config, mut published) = staged_inputs(&fixture);
        change_descriptor(&fixture, variant);
        let before = descriptor_fingerprints(&fixture);
        let protected = fixture.protected();
        assert!(
            service::reopen_staged(
                &fixture.owner,
                &mut config,
                &mut published,
                Some(&mut |_, _| Ok(())),
            )
            .is_err(),
            "variant {variant}"
        );
        assert_eq!(before, descriptor_fingerprints(&fixture));
        fixture.unchanged(protected);
    }
}

#[test]
fn reopened_handle_refuses_inventory_file_and_directory_policy_drift() {
    for variant in 0..12 {
        let fixture = fixture();
        let (mut config, mut published) = staged_inputs(&fixture);
        let mut descriptor = service::reopen_staged(
            &fixture.owner,
            &mut config,
            &mut published,
            Some(&mut |_, _| Ok(())),
        )
        .unwrap();
        change_descriptor(&fixture, variant);
        let before = descriptor_fingerprints(&fixture);
        let protected = fixture.protected();
        assert!(service::revalidate(&mut descriptor, &mut |_, _| Ok(())).is_err());
        assert!(service::revalidate(&mut descriptor, &mut |_, _| Ok(())).is_err());
        assert_eq!(before, descriptor_fingerprints(&fixture));
        fixture.unchanged(protected);
    }
}

#[test]
fn reopening_requires_runner_and_the_same_config_and_release_lock() {
    let fixture = fixture();
    let (config, mut published) = staged_inputs(&fixture);
    drop(config);
    fs::write(fixture.layout().config_file(), CONFIG).unwrap();
    let mut config = self::config(&fixture);
    let before = descriptor_fingerprints(&fixture);
    let protected = fixture.protected();
    assert_eq!(
        service::reopen_staged(
            &fixture.owner,
            &mut config,
            &mut published,
            Some(&mut |_, _| panic!("missing runner must reject before signer")),
        )
        .err(),
        Some("configured runner ID required")
    );

    let other = self::fixture();
    let (mut other_config, mut other_published) = staged_inputs(&other);
    let other_before = descriptor_fingerprints(&other);
    let other_protected = other.protected();
    assert_eq!(
        service::reopen_staged(
            &other.owner,
            &mut other_config,
            &mut published,
            Some(&mut |_, _| panic!("wrong release owner must reject before signer")),
        )
        .err(),
        Some("published release installer lock mismatch")
    );
    assert!(
        service::reopen_staged(
            &other.owner,
            &mut config,
            &mut other_published,
            Some(&mut |_, _| panic!("wrong config owner must reject before signer")),
        )
        .is_err()
    );
    assert_eq!(before, descriptor_fingerprints(&fixture));
    assert_eq!(other_before, descriptor_fingerprints(&other));
    fixture.unchanged(protected);
    other.unchanged(other_protected);
}

#[test]
fn reopening_and_retained_checks_refuse_signer_failure_without_writes() {
    for fail_at in [1, 4] {
        let fixture = fixture();
        let (mut config, mut published) = staged_inputs(&fixture);
        let before = descriptor_fingerprints(&fixture);
        let protected = fixture.protected();
        let mut calls = 0;
        assert_eq!(
            service::reopen_staged(
                &fixture.owner,
                &mut config,
                &mut published,
                Some(&mut |_, _| {
                    calls += 1;
                    if calls == fail_at {
                        Err("synthetic signer failure")
                    } else {
                        Ok(())
                    }
                }),
            )
            .err(),
            Some("synthetic signer failure")
        );
        assert_eq!(calls, fail_at);
        let mut descriptor = service::reopen_staged(
            &fixture.owner,
            &mut config,
            &mut published,
            Some(&mut |_, _| Ok(())),
        )
        .unwrap();
        assert_eq!(
            service::revalidate(&mut descriptor, &mut |_, _| Err("synthetic signer failure")),
            Err("synthetic signer failure")
        );
        assert_eq!(before, descriptor_fingerprints(&fixture));
        fixture.unchanged(protected);
    }
}

#[test]
fn replacement_during_reopening_is_not_admitted() {
    for parent in [false, true] {
        let fixture = fixture();
        let (mut config, mut published) = staged_inputs(&fixture);
        let protected = fixture.protected();
        let mut calls = 0;
        let mut replaced = None;
        assert!(
            service::reopen_staged(
                &fixture.owner,
                &mut config,
                &mut published,
                Some(&mut |_, _| {
                    calls += 1;
                    if calls == 4 {
                        replace_descriptor(&fixture, parent);
                        replaced = Some(descriptor_fingerprints(&fixture));
                    }
                    Ok(())
                }),
            )
            .is_err()
        );
        assert_eq!(replaced.unwrap(), descriptor_fingerprints(&fixture));
        fixture.unchanged(protected);
    }
}

#[test]
fn reopened_handle_rejects_replacement_before_or_during_revalidation() {
    for (parent, during) in [(false, false), (true, false), (false, true), (true, true)] {
        let fixture = fixture();
        let (mut config, mut published) = staged_inputs(&fixture);
        let protected = fixture.protected();
        let mut descriptor = service::reopen_staged(
            &fixture.owner,
            &mut config,
            &mut published,
            Some(&mut |_, _| Ok(())),
        )
        .unwrap();
        if !during {
            replace_descriptor(&fixture, parent);
        }
        let mut calls = 0;
        assert!(
            service::revalidate(&mut descriptor, &mut |_, _| {
                calls += 1;
                if during && calls == 1 {
                    replace_descriptor(&fixture, parent);
                }
                Ok(())
            })
            .is_err()
        );
        let replaced = descriptor_fingerprints(&fixture);
        assert!(service::revalidate(&mut descriptor, &mut |_, _| Ok(())).is_err());
        assert_eq!(replaced, descriptor_fingerprints(&fixture));
        fixture.unchanged(protected);
    }
}

#[test]
fn reopened_handle_preserves_original_config_lock_and_release_baselines() {
    for variant in 0..3 {
        let fixture = fixture();
        let (mut config, mut published) = staged_inputs(&fixture);
        let mut descriptor = service::reopen_staged(
            &fixture.owner,
            &mut config,
            &mut published,
            Some(&mut |_, _| Ok(())),
        )
        .unwrap();
        match variant {
            0 => fs::write(fixture.layout().config_file(), CONFIG).unwrap(),
            1 => {
                let path = fixture.layout().process_lock();
                fs::rename(&path, fixture._home.path().join("held-lock")).unwrap();
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
        let before = descriptor_fingerprints(&fixture);
        let protected = fixture.protected();
        assert!(service::revalidate(&mut descriptor, &mut |_, _| Ok(())).is_err());
        assert!(service::revalidate(&mut descriptor, &mut |_, _| Ok(())).is_err());
        assert_eq!(before, descriptor_fingerprints(&fixture));
        fixture.unchanged(protected);
    }
}
