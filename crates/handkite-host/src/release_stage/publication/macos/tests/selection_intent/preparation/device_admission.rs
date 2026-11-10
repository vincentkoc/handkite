use super::*;
use crate::selection_intent::CommitPhase;
use crate::selection_intent::macos::preparation::commit as transaction;

fn hooks<'a>(transport: device::Fake) -> Hooks<'a> {
    Hooks {
        device: Some(transport),
        nonce: Some([0x31; 16]),
        ..Hooks::production()
    }
}

#[tokio::test]
async fn prepare_and_commit_reobserve_once_after_signers_and_before_mutation() {
    let (fixture, bytes) = setup(Operation::Upgrade);
    let agreement = device::observe(&fixture.owner).await;
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &bytes);
    let transport = device::Fake::good();
    let events = transport.events();
    let signer_events = events.clone();
    let mut signer = |release: &mut PublishedRelease<'_>| {
        signer_events.borrow_mut().push("signer");
        check(release)
    };
    let mut before_link = |point| {
        if point == Point::BeforeLink {
            assert_eq!(
                &*events.borrow(),
                &["signer", "signer", "enumerate", "GetValue", "enumerate"]
            );
        }
        Ok(())
    };
    let prepared = prep::prepare(
        &fixture.owner,
        agreement,
        Operation::Upgrade,
        Some(&mut previous),
        &mut destination,
        &mut Hooks {
            check: Some(&mut signer),
            fault: Some(&mut before_link),
            ..hooks(transport)
        },
    )
    .await
    .unwrap();
    assert_eq!(
        events.borrow().iter().filter(|e| **e == "GetValue").count(),
        1
    );
    let transport = device::Fake::good();
    let events = transport.events();
    let signer_events = events.clone();
    let mut signer = |release: &mut PublishedRelease<'_>| {
        signer_events.borrow_mut().push("signer");
        check(release)
    };
    let mut before_rename = |point| {
        if point == transaction::Point::BeforeRename {
            assert_eq!(
                &*events.borrow(),
                &["signer", "signer", "enumerate", "GetValue", "enumerate"]
            );
        }
        Ok(())
    };
    let mut committed = transaction::commit(
        prepared,
        &mut transaction::Hooks {
            check: Some(&mut signer),
            fault: Some(&mut before_rename),
            device: Some(transport),
            ..transaction::Hooks::production()
        },
    )
    .await
    .unwrap();
    transaction::revalidate_for_test(&mut committed, &mut signer).unwrap();
    assert_eq!(
        events.borrow().iter().filter(|e| **e == "GetValue").count(),
        1
    );
}

#[tokio::test]
async fn changed_route_or_disconnect_refuses_prepare_without_creating_entries() {
    for transport in [device::Fake::changed_route(), device::Fake::disconnected()] {
        let (fixture, bytes) = setup(Operation::InitialInstall);
        let agreement = device::observe(&fixture.owner).await;
        let mut destination = open(&fixture.owner, &bytes);
        let before = protected(&fixture);
        let error = prep::prepare(
            &fixture.owner,
            agreement,
            Operation::InitialInstall,
            None,
            &mut destination,
            &mut Hooks {
                check: Some(&mut check),
                ..hooks(transport)
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.reason(), "device agreement unavailable or changed");
        assert!(error.residue_candidates().is_empty());
        config::no_preparation_entries(&fixture);
        fixture.unchanged(before);
    }
}

#[tokio::test]
async fn poisoned_or_cancelled_agreement_refuses_before_signers_or_transport() {
    for cancelled in [false, true] {
        let (fixture, bytes) = setup(Operation::InitialInstall);
        let mut agreement = device::observe(&fixture.owner).await;
        if cancelled {
            drop(device::reobserve(&mut agreement, device::Fake::good()));
        } else {
            assert!(
                device::reobserve(&mut agreement, device::Fake::disconnected())
                    .await
                    .is_err()
            );
        }
        let mut destination = open(&fixture.owner, &bytes);
        let transport = device::Fake::good();
        let events = transport.events();
        let before = protected(&fixture);
        let error = prep::prepare(
            &fixture.owner,
            agreement,
            Operation::InitialInstall,
            None,
            &mut destination,
            &mut Hooks {
                check: Some(&mut |_| panic!("poisoned agreement reached signer")),
                ..hooks(transport)
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.reason(), "device agreement unavailable or changed");
        assert!(error.residue_candidates().is_empty());
        assert!(events.borrow().is_empty());
        config::no_preparation_entries(&fixture);
        fixture.unchanged(before);
    }
}

#[tokio::test]
async fn changed_route_or_disconnect_before_commit_preserves_prepared_files() {
    for transport in [device::Fake::changed_route(), device::Fake::disconnected()] {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let prepared = prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
        let before = prepared_files(&fixture);
        let mut hooks = transaction::Hooks {
            check: Some(&mut check),
            device: Some(transport),
            ..transaction::Hooks::production()
        };
        let error = transaction::commit(prepared, &mut hooks)
            .await
            .err()
            .unwrap();
        assert_eq!(error.reason(), "device agreement unavailable or changed");
        assert_eq!(error.phase(), CommitPhase::BeforeCommit);
        assert_eq!(
            error.residue_candidates(),
            [CANDIDATE, "current", PENDING_JOURNAL]
        );
        assert_eq!(hooks.renames, 0);
        fixture.unchanged(before);
    }
}

fn prepared_files(fixture: &Fixture) -> Vec<(std::path::PathBuf, Fingerprint)> {
    let mut before = protected(fixture);
    for name in [CANDIDATE, PENDING_JOURNAL] {
        let path = fixture.layout().releases_directory().join(name);
        let metadata = fs::symlink_metadata(&path).unwrap().into();
        before.push((path, metadata));
    }
    before
}

#[tokio::test(start_paused = true)]
async fn cancellation_at_either_device_await_never_mutates_selection() {
    let (fixture, bytes) = setup(Operation::Upgrade);
    let agreement = device::observe(&fixture.owner).await;
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &bytes);
    let before = protected(&fixture);
    let transport = device::Fake::good().stalled();
    let events = transport.events();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            prep::prepare(
                &fixture.owner,
                agreement,
                Operation::Upgrade,
                Some(&mut previous),
                &mut destination,
                &mut Hooks {
                    check: Some(&mut check),
                    ..hooks(transport)
                },
            ),
        )
        .await
        .is_err()
    );
    assert_eq!(&*events.borrow(), &["enumerate", "GetValue"]);
    config::no_preparation_entries(&fixture);
    fixture.unchanged(before);

    let agreement = device::observe(&fixture.owner).await;
    let prepared = prepare(
        &fixture.owner,
        agreement,
        Operation::Upgrade,
        Some(&mut previous),
        &mut destination,
        &mut |_| Ok(()),
    )
    .await
    .unwrap();
    let before = prepared_files(&fixture);
    let transport = device::Fake::good().stalled();
    let events = transport.events();
    let mut hooks = transaction::Hooks {
        check: Some(&mut check),
        device: Some(transport),
        ..transaction::Hooks::production()
    };
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            transaction::commit(prepared, &mut hooks),
        )
        .await
        .is_err()
    );
    assert_eq!(&*events.borrow(), &["enumerate", "GetValue"]);
    assert_eq!(hooks.renames, 0);
    fixture.unchanged(before);
}

// All mutations happen during the Lockdown await, after the signer preflight.
fn drift(fixture: &Fixture, mutation: usize) -> impl FnMut() + 'static {
    let parent = fixture.layout().releases_directory();
    let config = fixture.layout().config_file();
    let lock = fixture.layout().process_lock();
    let artifact = fixture.directory().join(NAMES[0].filename());
    move || match mutation {
        0 => fs::write(&config, [CONFIG, b" "].concat()).unwrap(),
        1 => chmod(&lock, 0o644),
        2 | 3 => {
            let path = parent.join(if mutation == 2 { "current" } else { CANDIDATE });
            let target = fs::read_link(&path).unwrap();
            fs::rename(&path, parent.join("old-link")).unwrap();
            symlink(target, path).unwrap();
        }
        4 => {
            let path = parent.join(PENDING_JOURNAL);
            chmod(&path, 0o600);
            fs::write(&path, b"changed").unwrap();
            chmod(&path, 0o400);
        }
        5 => {
            chmod(&artifact, 0o700);
            fs::write(&artifact, b"changed").unwrap();
            chmod(&artifact, 0o500);
        }
        6 => fs::write(parent.join("unrelated"), b"changed").unwrap(),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn drift_during_prepare_observation_refuses_before_the_first_write() {
    for mutation in [0, 1, 2, 5, 6] {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let transport = device::Fake::good().during_reply(drift(&fixture, mutation));
        let error = prep::prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut Hooks {
                check: Some(&mut check),
                ..hooks(transport)
            },
        )
        .await
        .err()
        .unwrap();
        assert!(error.residue_candidates().is_empty(), "mutation {mutation}");
        config::no_preparation_entries(&fixture);
    }
}

#[tokio::test]
async fn drift_during_commit_observation_refuses_before_inventory_or_rename() {
    for mutation in 0..7 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let prepared = prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut |_| Ok(()),
        )
        .await
        .unwrap();
        let transport = device::Fake::good().during_reply(drift(&fixture, mutation));
        let mut hooks = transaction::Hooks {
            check: Some(&mut check),
            device: Some(transport),
            ..transaction::Hooks::production()
        };
        let error = transaction::commit(prepared, &mut hooks)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.phase(),
            CommitPhase::BeforeCommit,
            "mutation {mutation}"
        );
        assert_eq!(hooks.renames, 0);
        let parent = fixture.layout().releases_directory();
        assert!(fs::symlink_metadata(parent.join(CANDIDATE)).is_ok());
        assert!(fs::symlink_metadata(parent.join(PENDING_JOURNAL)).is_ok());
        assert_eq!(
            fs::read_link(parent.join("current")).unwrap(),
            Path::new(&fixture.name())
        );
    }
}
