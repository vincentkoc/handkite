use super::*;
use crate::device_agreement::tests::{Fake, observe};
use crate::launchd_descriptor::{RegistrationOutcome, staging::macos::registration};
use crate::launchd_observation::Observation as Job;

fn prepared() -> Fixture {
    let fixture = fixture();
    {
        let mut release = run(fixture.stage(), |_, _| Ok(())).unwrap();
        let mut config = config(&fixture);
        let mut check = |_, _: &Path| Ok(());
        service::stage(
            &fixture.owner,
            &mut config,
            &mut release,
            &mut hooks(&mut check),
        )
        .unwrap();
    }
    fs::remove_file(fixture.layout().current_release_selection()).unwrap();
    symlink(fixture.name(), fixture.layout().current_release_selection()).unwrap();
    let old = fixture.layout().releases_directory().join("99".repeat(32));
    fs::create_dir(&old).unwrap();
    chmod(&old, 0o700);
    fs::write(old.join("retained-controller"), b"preserved old release").unwrap();
    let parent = parent(&fixture);
    fs::create_dir(&parent).unwrap();
    chmod(&parent, 0o755);
    fs::write(parent.join("unrelated.plist"), b"preserved unrelated entry").unwrap();
    fixture
}

fn parent(fixture: &Fixture) -> std::path::PathBuf {
    fixture
        .layout()
        .registered_launchd_descriptor()
        .parent()
        .unwrap()
        .into()
}

fn published(fixture: &Fixture) -> PublishedRelease<'_> {
    reopen_published(
        &fixture.owner,
        &fixture.bytes,
        &hash(&fixture.bytes),
        CERTIFICATE,
        Some(&mut |_, _| Ok(())),
    )
    .unwrap()
}

fn registration_hooks<'a>(check: service::Check<'a>) -> registration::Hooks<'a> {
    registration::Hooks {
        files: hooks(check),
        device: Some(Fake::good()),
    }
}

#[tokio::test]
async fn publishes_once_and_verifies_existing_without_changing_inputs_or_unrelated_entries() {
    let fixture = prepared();
    let mut before = fixture.protected();
    for path in [
        fixture.layout().staged_launchd_descriptor(),
        fixture.receipt(),
        fixture.directory(),
        parent(&fixture).join("unrelated.plist"),
        fixture
            .layout()
            .releases_directory()
            .join("99".repeat(32))
            .join("retained-controller"),
    ] {
        before.push((path.clone(), fs::symlink_metadata(path).unwrap().into()));
    }
    let mut config = config(&fixture);
    let mut release = published(&fixture);
    let mut check = |_, _: &Path| Ok(());
    let mut hooks = registration_hooks(&mut check);
    let result = registration::register(
        &fixture.owner,
        observe(&fixture.owner).await,
        &mut config,
        &mut release,
        &mut hooks,
    )
    .await
    .unwrap();
    assert_eq!(result, RegistrationOutcome::Registered);
    assert_eq!(hooks.files.renames, 1);
    let path = fixture.layout().registered_launchd_descriptor();
    assert_eq!(
        fs::read(&path).unwrap(),
        fs::read(fixture.layout().staged_launchd_descriptor()).unwrap()
    );
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o400);
    let identity = Fingerprint::from(fs::symlink_metadata(&path).unwrap());
    let mut hooks = registration_hooks(&mut check);
    assert_eq!(
        registration::register(
            &fixture.owner,
            observe(&fixture.owner).await,
            &mut config,
            &mut release,
            &mut hooks
        )
        .await
        .unwrap(),
        RegistrationOutcome::VerifiedExisting
    );
    assert_eq!(hooks.files.renames, 0);
    assert_eq!(identity, fs::symlink_metadata(path).unwrap().into());
    fixture.unchanged(before);
}

#[tokio::test]
async fn missing_unsafe_or_replaced_parent_and_inexact_existing_entries_are_refused() {
    for case in 0..10 {
        let fixture = prepared();
        let parent = parent(&fixture);
        let path = fixture.layout().registered_launchd_descriptor();
        let bytes = fs::read(fixture.layout().staged_launchd_descriptor()).unwrap();
        match case {
            0 => {
                fs::remove_file(parent.join("unrelated.plist")).unwrap();
                fs::remove_dir(&parent).unwrap();
            }
            1 => chmod(&parent, 0o777),
            2 => {
                fs::rename(&parent, parent.with_extension("held")).unwrap();
                symlink(parent.with_extension("held"), &parent).unwrap();
            }
            3 => symlink(fixture.layout().staged_launchd_descriptor(), &path).unwrap(),
            4 => fs::create_dir(&path).unwrap(),
            _ => {
                fs::write(
                    &path,
                    if case == 5 {
                        b"different".as_slice()
                    } else {
                        &bytes
                    },
                )
                .unwrap();
                chmod(&path, if case == 6 { 0o600 } else { 0o400 });
                if case == 7 {
                    fs::hard_link(&path, parent.join("linked")).unwrap();
                }
                if case == 8 {
                    acl(&path);
                }
                if case == 9 {
                    assert!(
                        Command::new("/bin/chmod")
                            .args(["+a", "everyone allow write,add_file"])
                            .arg(&parent)
                            .status()
                            .unwrap()
                            .success()
                    );
                }
            }
        }
        let before = fixture.protected();
        let mut config = config(&fixture);
        let mut release = published(&fixture);
        let mut check = |_, _: &Path| Ok(());
        let mut hooks = registration_hooks(&mut check);
        let error = registration::register(
            &fixture.owner,
            observe(&fixture.owner).await,
            &mut config,
            &mut release,
            &mut hooks,
        )
        .await
        .unwrap_err();
        assert_eq!(error.phase(), StagePhase::BeforeRename, "case {case}");
        assert!(!error.has_residue());
        assert_eq!(hooks.files.renames, 0);
        fixture.unchanged(before);
    }
}

#[tokio::test]
async fn input_drift_or_gui_job_before_rename_never_publishes() {
    for case in 0..10 {
        let fixture = prepared();
        let mut config = config(&fixture);
        let mut release = published(&fixture);
        let agreement = observe(&fixture.owner).await;
        let mut check = |_, _: &Path| Ok(());
        let mut fault = |point| {
            if point == service::Point::BeforeRename {
                match case {
                    0 => {
                        let path = fixture.layout().current_release_selection();
                        fs::remove_file(&path).unwrap();
                        symlink(fixture.name(), path).unwrap();
                    }
                    1 => fs::write(fixture.layout().config_file(), RUNNER_CONFIG).unwrap(),
                    2 => chmod(&fixture.layout().staged_launchd_descriptor(), 0o600),
                    3 => fixture.owner.set_jobs_for_test([Job::Absent, Job::Loaded]),
                    4 => fixture.owner.set_jobs_for_test([Job::Absent, Job::Unknown]),
                    5 => {
                        fs::write(parent(&fixture).join("new-entry"), b"changed inventory").unwrap()
                    }
                    6 => {
                        let path = fixture.layout().process_lock();
                        fs::rename(&path, path.with_extension("held")).unwrap();
                        fs::write(&path, b"").unwrap();
                        chmod(&path, 0o600);
                    }
                    7 => {
                        let path = parent(&fixture);
                        fs::rename(&path, path.with_extension("held")).unwrap();
                        fs::create_dir(&path).unwrap();
                        chmod(&path, 0o755);
                    }
                    8 => {
                        let path = parent(&fixture).join(STAGE_TEMP);
                        fs::remove_file(&path).unwrap();
                        fs::write(
                            &path,
                            fs::read(fixture.layout().staged_launchd_descriptor()).unwrap(),
                        )
                        .unwrap();
                        chmod(&path, 0o400);
                    }
                    _ => acl(&parent(&fixture).join(STAGE_TEMP)),
                }
            }
            Ok(())
        };
        let mut hooks = registration_hooks(&mut check);
        hooks.files.fault = Some(&mut fault);
        let error = registration::register(
            &fixture.owner,
            agreement,
            &mut config,
            &mut release,
            &mut hooks,
        )
        .await
        .unwrap_err();
        assert_eq!(error.phase(), StagePhase::BeforeRename, "case {case}");
        assert!(error.has_residue());
        assert_eq!(hooks.files.renames, 0);
        assert!(!fixture.layout().registered_launchd_descriptor().exists());
    }
}

#[tokio::test]
async fn post_rename_drift_is_applied_and_never_retried() {
    for case in 0..4 {
        let fixture = prepared();
        let mut config = config(&fixture);
        let mut release = published(&fixture);
        let mut check = |_, _: &Path| Ok(());
        let mut fault = |point| {
            if point == service::Point::Complete {
                match case {
                    0 => chmod(&fixture.layout().registered_launchd_descriptor(), 0o600),
                    1 => fixture.owner.set_jobs_for_test([Job::Absent, Job::Loaded]),
                    2 => fs::write(parent(&fixture).join("raced-entry"), b"changed").unwrap(),
                    _ => {
                        let path = fixture.layout().current_release_selection();
                        fs::remove_file(&path).unwrap();
                        symlink(fixture.name(), &path).unwrap();
                    }
                }
            }
            Ok(())
        };
        let mut hooks = registration_hooks(&mut check);
        hooks.files.fault = Some(&mut fault);
        let error = registration::register(
            &fixture.owner,
            observe(&fixture.owner).await,
            &mut config,
            &mut release,
            &mut hooks,
        )
        .await
        .unwrap_err();
        assert_eq!(error.phase(), StagePhase::AppliedRename);
        assert!(error.has_residue());
        assert_eq!(hooks.files.renames, 1);
        assert!(fixture.layout().registered_launchd_descriptor().exists());
        assert!(!parent(&fixture).join(STAGE_TEMP).exists());
    }
}

#[tokio::test]
async fn wrong_selection_device_or_signer_is_refused_before_creation() {
    for case in 0..3 {
        let fixture = prepared();
        let mut config = config(&fixture);
        let mut release = published(&fixture);
        let agreement = observe(&fixture.owner).await;
        if case == 0 {
            let path = fixture.layout().current_release_selection();
            fs::remove_file(&path).unwrap();
            symlink(
                String::from_utf8(encode_target(&[0x99; 32]).to_vec()).unwrap(),
                &path,
            )
            .unwrap();
        }
        let before = fixture.protected();
        let mut check = |_, _: &Path| {
            if case == 2 {
                Err("synthetic signature failure")
            } else {
                Ok(())
            }
        };
        let mut hooks = registration_hooks(&mut check);
        if case == 1 {
            hooks.device = Some(Fake::changed_route());
        }
        let error = registration::register(
            &fixture.owner,
            agreement,
            &mut config,
            &mut release,
            &mut hooks,
        )
        .await
        .unwrap_err();
        assert_eq!(error.phase(), StagePhase::BeforeRename);
        assert!(!error.has_residue());
        assert_eq!(hooks.files.renames, 0);
        assert_eq!(fs::read_dir(parent(&fixture)).unwrap().count(), 1);
        fixture.unchanged(before);
    }
}

#[tokio::test]
async fn boundary_failures_preserve_precise_phase_and_residue_without_cleanup() {
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
        let fixture = prepared();
        let before = fixture.protected();
        let mut config = config(&fixture);
        let mut release = published(&fixture);
        let mut check = |_, _: &Path| Ok(());
        let mut fault = |at| {
            if at == point {
                Err("synthetic failure")
            } else {
                Ok(())
            }
        };
        let mut hooks = registration_hooks(&mut check);
        hooks.files.fault = Some(&mut fault);
        let error = registration::register(
            &fixture.owner,
            observe(&fixture.owner).await,
            &mut config,
            &mut release,
            &mut hooks,
        )
        .await
        .unwrap_err();
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
        assert_eq!(error.has_residue(), point != service::Point::BeforeCreate);
        assert_eq!(hooks.files.renames, usize::from(applied));
        assert_eq!(
            fixture.layout().registered_launchd_descriptor().exists(),
            applied
        );
        assert_eq!(
            parent(&fixture).join(STAGE_TEMP).exists(),
            error.has_residue() && !applied
        );
        fixture.unchanged(before);
    }
}

#[tokio::test]
async fn exclusive_rename_preserves_collision_and_reports_uncertain_syscalls() {
    for (code, applied) in [(libc::EEXIST, false), (libc::EIO, false), (libc::EIO, true)] {
        let fixture = prepared();
        let mut config = config(&fixture);
        let mut release = published(&fixture);
        let mut check = |_, _: &Path| Ok(());
        let mut fault = |point| {
            if code == libc::EEXIST && point == service::Point::BeforeRenameCall {
                fs::write(
                    fixture.layout().registered_launchd_descriptor(),
                    b"preserved collision",
                )
                .unwrap();
            }
            Ok(())
        };
        let mut hooks = registration_hooks(&mut check);
        hooks.files.fault = Some(&mut fault);
        if code != libc::EEXIST {
            hooks.files.rename_error = Some((code, applied));
        }
        let error = registration::register(
            &fixture.owner,
            observe(&fixture.owner).await,
            &mut config,
            &mut release,
            &mut hooks,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.phase(),
            if code == libc::EEXIST {
                StagePhase::BeforeRename
            } else {
                StagePhase::UncertainRename
            }
        );
        assert!(error.has_residue());
        assert_eq!(hooks.files.renames, 1);
        assert_eq!(parent(&fixture).join(STAGE_TEMP).exists(), !applied);
        if code == libc::EEXIST {
            assert_eq!(
                fs::read(fixture.layout().registered_launchd_descriptor()).unwrap(),
                b"preserved collision"
            );
        }
    }
}
