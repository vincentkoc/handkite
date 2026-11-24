use super::*;
use handkite_core::controller::{
    ActionAdmission, AdapterError, RawObservation, TargetEffect, TargetEffectReport,
};
use handkite_core::protocol::{Action, DeviceBinding, HardwareId, ScreenImage};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct Reports {
    app: String,
}

impl DeviceAdapter for Reports {
    fn identities(&mut self) -> Result<(HardwareId, HardwareId), AdapterError> {
        let id = HardwareId::new("fixture").unwrap();
        Ok((id.clone(), id))
    }
    fn observe(&mut self) -> Result<RawObservation, AdapterError> {
        self.observe_navigation()
    }
    fn observe_navigation(&mut self) -> Result<RawObservation, AdapterError> {
        let (connected_usb, wda_target) = self.identities()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        Ok(RawObservation {
            connected_usb,
            wda_target,
            window: ObservationWindow {
                started_unix_ms: now,
                finished_unix_ms: now,
            },
            state: DeviceState {
                lock: LockState::Unlocked,
                wda_ready: true,
                foreground_app: Some(self.app.clone()),
            },
            accessibility_tree: String::new(),
            screenshot: ScreenImage {
                format: ImageFormat::Png,
                bytes: vec![1],
            },
        })
    }
    fn prepare_navigation(
        &mut self,
        _: &Action,
    ) -> Result<(RawObservation, ActionAdmission), AdapterError> {
        Ok((
            self.observe_navigation()?,
            ActionAdmission::NativeHome {
                until: Instant::now() + Duration::from_secs(1),
            },
        ))
    }
    fn execute_navigation(
        &mut self,
        _: &Action,
        _: Instant,
        _: ActionAdmission,
        _: &str,
    ) -> Result<TransportReceipt, AdapterError> {
        self.app = "com.apple.springboard".into();
        Ok(TransportReceipt {
            accepted: Some(true),
            action_kind: "home".into(),
        })
    }
    fn execute(&mut self, _: &Action, _: Instant) -> Result<TransportReceipt, AdapterError> {
        panic!("wrong input route")
    }
    fn target_effect_updates(&mut self) -> Vec<TargetEffectReport> {
        vec![TargetEffectReport {
            update: TargetEffect {
                token: 7,
                target_id: format!("t_{}", "a".repeat(32)),
                effect: ObservedEffect::Unconfirmed,
                region: [1., 2., 3., 4.],
            },
            proof: None,
            completed_unix_ms: 0,
        }]
    }
}

#[test]
fn home_reply_does_not_carry_prior_tap_verdict_but_next_observation_does() {
    let key = DeviceKey::new("phone").unwrap();
    let controller = Controller::new(
        DeviceBinding {
            key: key.clone(),
            usb_id: HardwareId::new("fixture").unwrap(),
        },
        Reports {
            app: "example.app".into(),
        },
        5000,
        5000,
    );
    let before = controller.observe_navigation(&key).unwrap();
    let lease = controller
        .acquire("owner".into(), Duration::from_secs(30))
        .unwrap();
    let reply = dispatch(
        &controller,
        Request::Navigate {
            owner: "owner".into(),
            lease,
            request: ActionRequest {
                device: key.clone(),
                expected_revision: Some(before.revision),
                action: Action::Home,
            },
        },
    );
    let value = serde_json::to_value(reply).unwrap();
    assert_eq!(value["effect"], "changed");
    assert!(value.get("effect_updates").is_none());
    let next = serde_json::to_value(dispatch(
        &controller,
        Request::ObserveNavigation { device: key },
    ))
    .unwrap();
    assert_eq!(next["observation"]["effect_updates"][0]["token"], 7);
    assert_eq!(
        next["observation"]["effect_updates"][0]["effect"],
        "unconfirmed"
    );
}
