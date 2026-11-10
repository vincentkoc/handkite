use super::super::device::Fake;
use super::*;

#[tokio::test]
async fn reobserves_the_original_device_once_after_signers_before_rename() {
    let (fixture, bytes) = setup(Operation::Upgrade);
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &bytes);
    let value = committed(
        &fixture.owner,
        Operation::Upgrade,
        Some(&mut previous),
        &mut destination,
        0x31,
    )
    .await;
    let transport = Fake::good();
    let events = transport.events();
    let signer_events = events.clone();
    let mut signer = |release: &mut PublishedRelease<'_>| {
        signer_events.borrow_mut().push("signer");
        check(release)
    };
    let mut checkpoint = |point| {
        if point == RetirementPoint::BeforeRename {
            assert_eq!(
                &*events.borrow(),
                &["signer", "signer", "enumerate", "GetValue", "enumerate"]
            );
        }
        Ok(())
    };
    let mut hooks = RetirementHooks {
        check: Some(&mut signer),
        device: Some(transport),
        fault: Some(&mut checkpoint),
        ..RetirementHooks::production()
    };
    let mut retired = retirement::retire(value, &mut hooks).await.unwrap();
    assert_eq!(hooks.renames, 1);
    retirement::revalidate_for_test(&mut retired, &mut check).unwrap();
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|event| **event == "GetValue")
            .count(),
        1
    );
}

#[tokio::test]
async fn disconnected_or_changed_device_preserves_every_committed_name() {
    for transport in [Fake::disconnected(), Fake::changed_route()] {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let value = committed(
            &fixture.owner,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            0x31,
        )
        .await;
        let before = inventory(&fixture);
        let mut hooks = RetirementHooks {
            check: Some(&mut check),
            device: Some(transport),
            ..RetirementHooks::production()
        };
        let error = retirement::retire(value, &mut hooks).await.err().unwrap();
        assert_eq!(error.phase(), RetirementPhase::Before);
        assert_eq!(hooks.renames, 0);
        fixture.unchanged(before);
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_device_observation_preserves_every_name() {
    let (fixture, bytes) = setup(Operation::Upgrade);
    let mut previous = open(&fixture.owner, &fixture.bytes);
    let mut destination = open(&fixture.owner, &bytes);
    let value = committed(
        &fixture.owner,
        Operation::Upgrade,
        Some(&mut previous),
        &mut destination,
        0x31,
    )
    .await;
    let before = inventory(&fixture);
    let transport = Fake::good().stalled();
    let events = transport.events();
    let mut hooks = RetirementHooks {
        check: Some(&mut check),
        device: Some(transport),
        ..RetirementHooks::production()
    };
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            retirement::retire(value, &mut hooks)
        )
        .await
        .is_err()
    );
    assert_eq!(&*events.borrow(), &["enumerate", "GetValue"]);
    assert_eq!(hooks.renames, 0);
    fixture.unchanged(before);
}

#[tokio::test]
async fn drift_during_device_await_refuses_before_retirement() {
    for mutation in 0..12 {
        let (fixture, bytes) = setup(Operation::Upgrade);
        let parent = fixture.layout().releases_directory();
        fs::write(parent.join("unrelated"), b"baseline").unwrap();
        let mut previous = open(&fixture.owner, &fixture.bytes);
        let mut destination = open(&fixture.owner, &bytes);
        let value = committed(
            &fixture.owner,
            Operation::Upgrade,
            Some(&mut previous),
            &mut destination,
            0x31,
        )
        .await;
        let transport = Fake::good().during_reply(failures::drift(&fixture, mutation, false));
        let mut hooks = RetirementHooks {
            check: Some(&mut check),
            device: Some(transport),
            ..RetirementHooks::production()
        };
        let error = retirement::retire(value, &mut hooks).await.err().unwrap();
        assert_eq!(error.phase(), RetirementPhase::Before);
        assert_eq!(hooks.renames, 0);
        assert!(parent.join(PENDING_JOURNAL).is_file());
        assert!(fs::symlink_metadata(parent.join(COMPLETED)).is_err());
    }
}
