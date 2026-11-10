use super::*;
use crate::release_files::Fingerprint;
use crate::service_layout::{ServiceLayout, tests::fixture_in};
use handkite_core::protocol::{DeviceBinding, DeviceKey, HardwareId};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::{cell::RefCell, collections::VecDeque, fs, path::Path, rc::Rc, time::Duration};

const CONFIG: &[u8] = br#"{"device_key":"phone","usb_id":"test-device"}"#;

fn expected() -> DeviceBinding {
    DeviceBinding {
        key: DeviceKey::new("phone").unwrap(),
        usb_id: HardwareId::new("test-device").unwrap(),
    }
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn fixture() -> (tempfile::TempDir, ServiceLayout) {
    let value = fixture_in(&std::env::current_dir().unwrap());
    fs::write(value.1.config_file(), CONFIG).unwrap();
    chmod(&value.1.config_file(), 0o600);
    for name in [
        "UNLOCK-DISABLED",
        "pairing.fixture",
        "signing.fixture",
        "state.json",
    ] {
        fs::write(
            value.1.state_directory().join(name),
            b"preserved-private-state",
        )
        .unwrap();
    }
    fs::write(value.1.staged_launchd_descriptor(), b"preserved-descriptor").unwrap();
    symlink("ab".repeat(32), value.1.current_release_selection()).unwrap();
    value
}

fn protected(layout: &ServiceLayout) -> Vec<(std::path::PathBuf, Fingerprint)> {
    let mut paths = vec![
        layout.config_file(),
        layout.process_lock(),
        layout.current_release_selection(),
        layout.staged_launchd_descriptor(),
    ];
    paths.extend(
        fs::read_dir(layout.state_directory())
            .unwrap()
            .map(|entry| entry.unwrap().path()),
    );
    paths
        .into_iter()
        .map(|path| {
            let metadata = fs::symlink_metadata(&path).unwrap().into();
            (path, metadata)
        })
        .collect()
}

fn unchanged(before: Vec<(std::path::PathBuf, Fingerprint)>) {
    for (path, original) in before {
        assert_eq!(original, fs::symlink_metadata(path).unwrap().into());
    }
}

struct Fake {
    routes: VecDeque<Result<Route, &'static str>>,
    reply: Result<String, &'static str>,
    events: Rc<RefCell<Vec<&'static str>>>,
    after_reply: Option<Box<dyn FnMut()>>,
    delay: Duration,
    stall: bool,
}

impl Fake {
    fn good() -> Self {
        Self {
            routes: [
                Ok(Route::fixture("test-device", 7)),
                Ok(Route::fixture("test-device", 7)),
            ]
            .into(),
            reply: Ok("test-device".into()),
            events: Rc::default(),
            after_reply: None,
            delay: Duration::ZERO,
            stall: false,
        }
    }
}

impl Transport for Fake {
    async fn enumerate(&mut self, expected: &HardwareId) -> Result<Route, &'static str> {
        assert_eq!(expected.as_str(), "test-device");
        self.events.borrow_mut().push("enumerate");
        tokio::time::sleep(self.delay).await;
        self.routes.pop_front().unwrap()
    }

    async fn unique_id(&mut self, route: &Route) -> Result<String, &'static str> {
        assert_eq!(route.udid(), "test-device");
        self.events.borrow_mut().push("GetValue");
        if self.stall {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(self.delay).await;
        if let Some(hook) = &mut self.after_reply {
            hook();
        }
        self.reply.clone()
    }
}

#[tokio::test]
async fn borrows_exact_owner_config_and_retains_route_without_changing_state() {
    let (_scratch, layout) = fixture();
    let owner = InstallerLock::acquire(&layout).unwrap();
    let mut config = RetainedConfig::acquire(&owner, &expected()).unwrap();
    let before = protected(&layout);
    let mut transport = Fake::good();
    let events = transport.events.clone();
    let mut agreement = DeviceAgreement::observe_with(&owner, &mut config, &mut transport)
        .await
        .unwrap();
    assert!(std::ptr::eq(agreement.owner, &owner));
    assert!(agreement.route == Route::fixture("test-device", 7));
    assert_eq!(&*events.borrow(), &["enumerate", "GetValue", "enumerate"]);
    for _ in 0..2 {
        agreement.reobserve_with(Fake::good()).await.unwrap();
    }
    drop(agreement);
    config.revalidate().unwrap();
    unchanged(before);
}

#[tokio::test]
async fn different_owner_is_rejected_before_transport() {
    let (_scratch, layout) = fixture();
    let (_other_scratch, other_layout) = fixture();
    let owner = InstallerLock::acquire(&layout).unwrap();
    let other = InstallerLock::acquire(&other_layout).unwrap();
    let mut config = RetainedConfig::acquire(&owner, &expected()).unwrap();
    let mut transport = Fake::good();
    assert_eq!(
        DeviceAgreement::observe_with(&other, &mut config, &mut transport)
            .await
            .err(),
        Some(UNAVAILABLE)
    );
    assert!(transport.events.borrow().is_empty());
}

#[tokio::test]
async fn missing_ambiguous_mismatching_and_replaced_routes_or_replies_fail() {
    for case in 0..8 {
        let (_scratch, layout) = fixture();
        let owner = InstallerLock::acquire(&layout).unwrap();
        let mut config = RetainedConfig::acquire(&owner, &expected()).unwrap();
        let before = protected(&layout);
        let mut transport = Fake::good();
        match case {
            0 => transport.routes[0] = Err(UNAVAILABLE),
            1 => transport.routes[0] = Ok(Route::fixture("other", 7)),
            2 => transport.reply = Ok("TEST-device".into()),
            3 => transport.reply = Ok("test-device ".into()),
            4 => transport.reply = Err(UNAVAILABLE),
            5 => transport.routes[1] = Err(UNAVAILABLE),
            6 => transport.routes[1] = Ok(Route::fixture("test-device", 8)),
            _ => transport.routes[1] = Ok(Route::fixture("other", 7)),
        }
        assert_eq!(
            DeviceAgreement::observe_with(&owner, &mut config, &mut transport)
                .await
                .err(),
            Some(UNAVAILABLE)
        );
        unchanged(before);
    }
}

#[tokio::test]
async fn config_and_lock_drift_before_or_during_observation_fail() {
    for during in [false, true] {
        for case in 0..5 {
            let (_scratch, layout) = fixture();
            let owner = InstallerLock::acquire(&layout).unwrap();
            let mut config = RetainedConfig::acquire(&owner, &expected()).unwrap();
            let mut transport = Fake::good();
            let changed_layout = layout.clone();
            let mut change: Box<dyn FnMut()> = Box::new(move || {
                let path = changed_layout.config_file();
                match case {
                    0 => fs::write(&path, [CONFIG, b" "].concat()).unwrap(),
                    1 => {
                        fs::rename(&path, path.with_extension("original")).unwrap();
                        fs::write(&path, CONFIG).unwrap();
                        chmod(&path, 0o600);
                    }
                    2 => chmod(&path, 0o644),
                    3 => chmod(&changed_layout.process_lock(), 0o644),
                    _ => {
                        let path = changed_layout.process_lock();
                        fs::rename(&path, path.with_extension("original")).unwrap();
                        fs::write(&path, b"").unwrap();
                        chmod(&path, 0o600);
                    }
                }
            });
            if during {
                transport.after_reply = Some(change);
            } else {
                change();
            }
            assert_eq!(
                DeviceAgreement::observe_with(&owner, &mut config, &mut transport)
                    .await
                    .err(),
                Some(UNAVAILABLE)
            );
            assert_eq!(transport.events.borrow().is_empty(), !during);
        }
    }
}

#[tokio::test]
async fn reobservation_never_rebases_the_route_and_failure_is_permanent() {
    let (_scratch, layout) = fixture();
    let owner = InstallerLock::acquire(&layout).unwrap();
    let mut config = RetainedConfig::acquire(&owner, &expected()).unwrap();
    let mut agreement = DeviceAgreement::observe_with(&owner, &mut config, &mut Fake::good())
        .await
        .unwrap();
    let mut replacement = Fake::good();
    replacement.routes[0] = Ok(Route::fixture("test-device", 8));
    let events = replacement.events.clone();
    assert_eq!(
        agreement.reobserve_with(replacement).await,
        Err(UNAVAILABLE)
    );
    assert_eq!(&*events.borrow(), &["enumerate"]);
    assert!(agreement.route == Route::fixture("test-device", 7));
    let good = Fake::good();
    let events = good.events.clone();
    assert_eq!(agreement.reobserve_with(good).await, Err(UNAVAILABLE));
    assert!(events.borrow().is_empty());
}

#[tokio::test]
async fn cancellation_including_an_unpolled_future_permanently_poisons_handle() {
    for polled in [false, true] {
        let (_scratch, layout) = fixture();
        let owner = InstallerLock::acquire(&layout).unwrap();
        let mut config = RetainedConfig::acquire(&owner, &expected()).unwrap();
        let before = protected(&layout);
        let mut agreement = DeviceAgreement::observe_with(&owner, &mut config, &mut Fake::good())
            .await
            .unwrap();
        let mut stalled = Fake::good();
        stalled.stall = true;
        let events = stalled.events.clone();
        let mut future = Box::pin(agreement.reobserve_with(stalled));
        if polled {
            tokio::select! {
                _ = &mut future => panic!("stalled observation unexpectedly completed"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            assert_eq!(&*events.borrow(), &["enumerate", "GetValue"]);
        }
        drop(future);
        assert_eq!(
            agreement.reobserve_with(Fake::good()).await,
            Err(UNAVAILABLE)
        );
        drop(agreement);
        config.revalidate().unwrap();
        unchanged(before);
    }
}

#[tokio::test(start_paused = true)]
async fn one_absolute_deadline_covers_all_transport_steps_and_poisons_reobserve() {
    let (_scratch, layout) = fixture();
    let owner = InstallerLock::acquire(&layout).unwrap();
    let mut config = RetainedConfig::acquire(&owner, &expected()).unwrap();
    let mut agreement = DeviceAgreement::observe_with(&owner, &mut config, &mut Fake::good())
        .await
        .unwrap();
    let mut slow = Fake::good();
    slow.delay = Duration::from_secs(6);
    let start = tokio::time::Instant::now();
    assert_eq!(agreement.reobserve_with(slow).await, Err(UNAVAILABLE));
    assert_eq!(tokio::time::Instant::now() - start, OBSERVATION_TIMEOUT);
    assert_eq!(
        agreement.reobserve_with(Fake::good()).await,
        Err(UNAVAILABLE)
    );
}

#[tokio::test]
async fn cancelled_initial_observation_leaves_no_handle_or_persistent_changes() {
    let (_scratch, layout) = fixture();
    let owner = InstallerLock::acquire(&layout).unwrap();
    let mut config = RetainedConfig::acquire(&owner, &expected()).unwrap();
    let before = protected(&layout);
    let mut stalled = Fake::good();
    stalled.stall = true;
    {
        let future = DeviceAgreement::observe_with(&owner, &mut config, &mut stalled);
        tokio::select! {
            _ = future => panic!("stalled observation unexpectedly completed"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }
    config.revalidate().unwrap();
    unchanged(before);
}
