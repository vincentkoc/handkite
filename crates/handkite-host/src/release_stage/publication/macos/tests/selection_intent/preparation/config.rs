use super::*;

fn change_config(fixture: &Fixture, mutation: u8) {
    let path = fixture.layout().config_file();
    match mutation {
        0 => fs::write(&path, [CONFIG, b" "].concat()).unwrap(),
        1 => {
            fs::rename(&path, path.with_extension("original")).unwrap();
            fs::write(&path, CONFIG).unwrap();
            chmod(&path, 0o600);
        }
        2 => chmod(&path, 0o644),
        3 => acl(&path),
        _ => unreachable!(),
    }
}

fn preserved(fixture: &Fixture) -> Vec<(std::path::PathBuf, Fingerprint)> {
    unchanged(fixture)
        .into_iter()
        .filter(|(path, _)| path != &fixture.layout().config_file())
        .collect()
}

pub(super) fn no_preparation_entries(fixture: &Fixture) {
    let parent = fixture.layout().releases_directory();
    assert!(fs::symlink_metadata(parent.join(CANDIDATE)).is_err());
    assert!(fs::symlink_metadata(parent.join(PENDING_JOURNAL)).is_err());
}

#[tokio::test]
async fn config_from_another_lock_refuses_public_preparation_without_mutation() {
    let (fixture, bytes) = setup(Operation::Upgrade);
    let other = Fixture::new();
    let agreement = device::observe(&other.owner).await;
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &bytes);
    let before = protected(&fixture);
    let other_before = other.protected();
    let error = intent::prepare(
        &fixture.owner,
        agreement,
        Operation::Upgrade,
        Some(&mut previous),
        &mut destination,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.reason(), "device agreement unavailable or changed");
    assert!(error.residue_candidates().is_empty());
    no_preparation_entries(&fixture);
    fixture.unchanged(before);
    other.unchanged(other_before);
    assert!(
        device::observe(&other.owner)
            .await
            .revalidate_for_owner(&other.owner)
            .is_ok()
    );
}

#[tokio::test]
async fn already_changed_config_refuses_before_any_write_or_signer_check() {
    for mutation in 0..4 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let agreement = device::observe(&fixture.owner).await;
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        change_config(&fixture, mutation);
        let before = protected(&fixture);
        let error = prep::prepare(
            &fixture.owner,
            agreement,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            &mut Hooks {
                device: Some(device::Fake::good()),
                check: Some(&mut |_| panic!("changed agreement reached signer verification")),
                fault: None,
                nonce: Some([0x31; 16]),
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.reason(), "unsafe or changed service configuration");
        assert!(error.residue_candidates().is_empty());
        no_preparation_entries(&fixture);
        fixture.unchanged(before);
    }
}

#[tokio::test]
async fn config_drift_at_each_preparation_boundary_preserves_protected_paths_and_residues() {
    for point in [
        Point::BeforeLink,
        Point::LinkCreated,
        Point::LinkSynced,
        Point::BeforeJournal,
        Point::JournalCreated,
        Point::JournalWritten,
        Point::JournalReadonly,
        Point::JournalSynced,
        Point::BeforeFinalVerify,
        Point::Complete,
    ] {
        for mutation in 0..4 {
            let (fixture, bytes) = setup(Operation::Upgrade);
            let agreement = device::observe(&fixture.owner).await;
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let before = preserved(&fixture);
            let parent = fixture.layout().releases_directory();
            let parent_before = Fingerprint::from(fs::metadata(&parent).unwrap());
            let error = prepare(
                &fixture.owner,
                agreement,
                Operation::Upgrade,
                Some(&mut previous),
                &mut destination,
                &mut |at| {
                    if at == point {
                        change_config(&fixture, mutation);
                    }
                    Ok(())
                },
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.reason(), "unsafe or changed service configuration");
            let expected = match point {
                Point::BeforeLink => 0,
                Point::LinkCreated | Point::LinkSynced | Point::BeforeJournal => 1,
                _ => 2,
            };
            assert_eq!(error.residue_candidates().len(), expected, "{point:?}");
            for name in error.residue_candidates() {
                assert!(fs::symlink_metadata(parent.join(name)).is_ok());
            }
            if expected == 0 {
                no_preparation_entries(&fixture);
                assert_eq!(parent_before, fs::metadata(parent).unwrap().into());
            }
            fixture.unchanged(before);
        }
    }
}

#[tokio::test]
async fn config_drift_during_initial_or_final_signer_checks_refuses_preparation() {
    for change_at in 1..=4 {
        for mutation in 0..4 {
            let (fixture, bytes) = setup(Operation::Upgrade);
            let agreement = device::observe(&fixture.owner).await;
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let before = preserved(&fixture);
            let mut calls = 0;
            let error = prep::prepare(
                &fixture.owner,
                agreement,
                Operation::Upgrade,
                Some(&mut previous),
                &mut destination,
                &mut Hooks {
                    device: Some(device::Fake::good()),
                    check: Some(&mut |release| {
                        check(release)?;
                        calls += 1;
                        if calls == change_at {
                            change_config(&fixture, mutation);
                        }
                        Ok(())
                    }),
                    fault: None,
                    nonce: Some([0x31; 16]),
                },
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.reason(), "unsafe or changed service configuration");
            assert!(calls >= change_at);
            assert_eq!(
                error.residue_candidates().len(),
                if change_at <= 2 { 0 } else { 2 }
            );
            if change_at <= 2 {
                no_preparation_entries(&fixture);
            }
            fixture.unchanged(before);
        }
    }
}

#[tokio::test]
async fn retained_preparation_rejects_config_drift_before_or_during_signer_checks() {
    for change_at in 0..=2 {
        for mutation in 0..4 {
            let (fixture, bytes) = setup(Operation::Upgrade);
            let agreement = device::observe(&fixture.owner).await;
            let mut previous = open(&fixture.owner, &fixture.bytes);
            let mut destination = open(&fixture.owner, &bytes);
            let mut value = prepare(
                &fixture.owner,
                agreement,
                Operation::Upgrade,
                Some(&mut previous),
                &mut destination,
                &mut |_| Ok(()),
            )
            .await
            .unwrap();
            let before = preserved(&fixture);
            let parent = fixture.layout().releases_directory();
            let journal = fs::read(parent.join(PENDING_JOURNAL)).unwrap();
            if change_at == 0 {
                change_config(&fixture, mutation);
                assert_eq!(
                    value.revalidate(),
                    Err("unsafe or changed service configuration")
                );
            } else {
                let mut calls = 0;
                assert_eq!(
                    prep::revalidate_for_test(&mut value, &mut |release| {
                        check(release)?;
                        calls += 1;
                        if calls == change_at {
                            change_config(&fixture, mutation);
                        }
                        Ok(())
                    }),
                    Err("unsafe or changed service configuration")
                );
                assert!(calls >= change_at);
            }
            for _ in 0..2 {
                assert_eq!(
                    prep::revalidate_for_test(&mut value, &mut |_| panic!(
                        "agreement was rebaselined"
                    )),
                    Err("unsafe or changed service configuration")
                );
            }
            assert_eq!(fs::read(parent.join(PENDING_JOURNAL)).unwrap(), journal);
            assert!(fs::symlink_metadata(parent.join(CANDIDATE)).is_ok());
            fixture.unchanged(before);
        }
    }
}
