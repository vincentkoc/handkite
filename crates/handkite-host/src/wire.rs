use base64::Engine;
use handkite_core::controller::{Controller, ControllerError, DeviceAdapter, LeaseId, ViewChange};
use handkite_core::protocol::{
    ActionRequest, CaptureMode, DeviceKey, DeviceState, ImageFormat, ImageObservation, LockState,
    Observation, ObservationWindow, ObservedEffect, Revision, TransportReceipt,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    ObserveAlert {
        device: DeviceKey,
    },
    TapAlert {
        owner: String,
        lease: LeaseId,
        device: DeviceKey,
        expected_revision: Revision,
        alert_id: String,
        target_id: String,
    },
    ObserveNavigation {
        device: DeviceKey,
    },
    Navigate {
        owner: String,
        lease: LeaseId,
        request: ActionRequest,
    },
    Status {
        device: DeviceKey,
    },
    ObserveTargets {
        device: DeviceKey,
        #[serde(default)]
        since_revision: Option<Revision>,
    },
    ActElement {
        owner: String,
        lease: LeaseId,
        device: DeviceKey,
        expected_revision: Revision,
        action: handkite_core::controller::ElementAction,
    },
    Observe {
        device: DeviceKey,
    },
    ObserveOutline {
        device: DeviceKey,
    },
    ObserveImage {
        device: DeviceKey,
    },
    Acquire {
        owner: String,
        ttl_ms: u64,
        #[serde(default)]
        renew_on_success: bool,
    },
    Release {
        owner: String,
        lease: LeaseId,
    },
    Act {
        owner: String,
        lease: LeaseId,
        request: ActionRequest,
        #[serde(default)]
        capture: CaptureMode,
        /// Opt-in motion shaping; absent unless the client paces this call.
        #[serde(default)]
        pacing: Option<handkite_core::protocol::Motion>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    ObservedAlert {
        status: WireStatus,
        alert: handkite_core::controller::AlertDescriptor,
    },
    AlertActed {
        receipt: TransportReceipt,
        effect: ObservedEffect,
    },
    ObservedNavigation {
        observation: WireObservation,
    },
    Navigated {
        receipt: TransportReceipt,
        effect: ObservedEffect,
        after: Option<WireObservation>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        effect_updates: Vec<handkite_core::controller::TargetEffect>,
    },
    Status {
        status: WireStatus,
    },
    ElementActed {
        receipt: TransportReceipt,
        effect: ObservedEffect,
        next: Option<WireObservation>,
        targets: Vec<handkite_core::controller::ElementTarget>,
        after_image: Option<WireImageObservation>,
        #[serde(default)]
        effect_updates: Vec<handkite_core::controller::TargetEffect>,
    },
    ObservedTargets {
        observation: WireObservation,
        targets: Vec<handkite_core::controller::ElementTarget>,
    },
    Observed {
        observation: WireObservation,
    },
    ObservedOutline {
        observation: WireObservation,
        navigation_only: bool,
    },
    ObservedImage {
        observation: WireImageObservation,
    },
    Acquired {
        lease: LeaseId,
    },
    Released,
    Acted {
        receipt: TransportReceipt,
        after: Option<WireObservation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_image: Option<WireImageObservation>,
        native_lock: Option<LockState>,
        effect: ObservedEffect,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        effect_updates: Vec<handkite_core::controller::TargetEffect>,
    },
    Error {
        code: String,
    },
    ViewChanged {
        changed: ViewChange,
    },
}

#[derive(Serialize, Deserialize)]
pub struct WireStatus {
    pub device: DeviceKey,
    pub revision: Revision,
    pub window: ObservationWindow,
    pub state: DeviceState,
}

impl From<Observation> for WireStatus {
    fn from(value: Observation) -> Self {
        Self {
            device: value.device,
            revision: value.revision,
            window: value.window,
            state: value.state,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct WireObservation {
    pub device: DeviceKey,
    pub revision: Revision,
    pub window: ObservationWindow,
    pub state: DeviceState,
    pub accessibility_tree: String,
    pub screenshot_format: ImageFormat,
    pub screenshot_base64: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effect_updates: Vec<handkite_core::controller::TargetEffect>,
}

#[derive(Serialize, Deserialize)]
pub struct WireImageObservation {
    pub device: DeviceKey,
    pub window: ObservationWindow,
    pub screenshot_format: ImageFormat,
    pub screenshot_base64: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effect_updates: Vec<handkite_core::controller::TargetEffect>,
}

impl From<ImageObservation> for WireImageObservation {
    fn from(value: ImageObservation) -> Self {
        let _timing = crate::timing::Span::new("wire_image_encode");
        Self {
            device: value.device,
            window: value.window,
            screenshot_format: value.screenshot.format,
            screenshot_base64: base64::engine::general_purpose::STANDARD
                .encode(value.screenshot.bytes),
            effect_updates: Vec::new(),
        }
    }
}

impl From<Observation> for WireObservation {
    fn from(value: Observation) -> Self {
        let _timing = crate::timing::Span::new("wire_image_encode");
        Self {
            device: value.device,
            revision: value.revision,
            window: value.window,
            state: value.state,
            accessibility_tree: value.accessibility_tree,
            screenshot_format: value.screenshot.format,
            screenshot_base64: base64::engine::general_purpose::STANDARD
                .encode(value.screenshot.bytes),
            effect_updates: Vec::new(),
        }
    }
}

impl From<handkite_core::controller::NavigationObservation> for WireObservation {
    fn from(value: handkite_core::controller::NavigationObservation) -> Self {
        Self {
            device: value.device,
            revision: value.revision,
            window: value.window,
            state: value.state,
            accessibility_tree: String::new(),
            screenshot_format: value.screenshot.format,
            screenshot_base64: base64::engine::general_purpose::STANDARD
                .encode(value.screenshot.bytes),
            effect_updates: Vec::new(),
        }
    }
}

fn valid_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 64
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

pub fn dispatch<A: DeviceAdapter>(controller: &Controller<A>, request: Request) -> Reply {
    let _timing = crate::timing::Span::new("controller_dispatch");
    let result = match request {
        Request::ObserveAlert { device } => {
            controller
                .observe_alert(&device)
                .map(|result| Reply::ObservedAlert {
                    status: result.observation.into(),
                    alert: result.alert,
                })
        }
        Request::TapAlert {
            owner,
            lease,
            device,
            expected_revision,
            alert_id,
            target_id,
        } if valid_owner(&owner) => controller
            .tap_alert(
                &owner,
                lease,
                &device,
                expected_revision,
                handkite_core::controller::AlertRequest {
                    alert_id,
                    target_id,
                },
            )
            .map(|result| Reply::AlertActed {
                receipt: result.receipt,
                effect: result.effect,
            }),
        Request::ObserveNavigation { device } => {
            controller
                .observe_navigation(&device)
                .map(|observation| Reply::ObservedNavigation {
                    observation: observation.into(),
                })
        }
        Request::Navigate {
            owner,
            lease,
            request,
        } if valid_owner(&owner) => {
            controller
                .navigate(&owner, lease, request)
                .map(|result| Reply::Navigated {
                    effect_updates: Vec::new(),
                    receipt: result.receipt,
                    effect: result.effect,
                    after: result.after.map(Into::into),
                })
        }
        Request::Status { device } => {
            controller
                .observe_navigation(&device)
                .map(|observation| Reply::Status {
                    status: WireStatus {
                        device: observation.device,
                        revision: observation.revision,
                        window: observation.window,
                        state: observation.state,
                    },
                })
        }
        Request::ObserveTargets {
            device,
            since_revision,
        } => controller
            .observe_targets_since(&device, since_revision)
            .map(|result| Reply::ObservedTargets {
                observation: result.observation.into(),
                targets: result.targets,
            }),
        Request::ActElement {
            owner,
            lease,
            device,
            expected_revision,
            action,
        } if valid_owner(&owner) => controller
            .act_element(&owner, lease, &device, expected_revision, action)
            .map(|outcome| {
                let (next, targets) = outcome
                    .next
                    .map(|next| (Some(next.observation.into()), next.targets))
                    .unwrap_or_default();
                Reply::ElementActed {
                    receipt: outcome.result.receipt,
                    effect: outcome.result.effect,
                    next,
                    targets,
                    after_image: outcome.result.after_image.map(Into::into),
                    effect_updates: Vec::new(),
                }
            }),
        Request::ObserveOutline { device } => {
            controller
                .observe_outline(&device)
                .map(|result| Reply::ObservedOutline {
                    observation: result.observation.into(),
                    navigation_only: result.navigation_only,
                })
        }
        Request::Observe { device } => {
            controller
                .observe(&device)
                .map(|observation| Reply::Observed {
                    observation: observation.into(),
                })
        }
        Request::ObserveImage { device } => {
            controller
                .observe_image(&device)
                .map(|observation| Reply::ObservedImage {
                    observation: observation.into(),
                })
        }
        Request::Acquire {
            owner,
            ttl_ms,
            renew_on_success,
        } if valid_owner(&owner) => controller
            .acquire_with_renewal(owner, Duration::from_millis(ttl_ms), renew_on_success)
            .map(|lease| Reply::Acquired { lease }),
        Request::Release { owner, lease } if valid_owner(&owner) => {
            controller.release(&owner, lease).map(|()| Reply::Released)
        }
        Request::Act {
            owner,
            lease,
            request,
            capture,
            pacing,
        } if valid_owner(&owner) => controller
            .act_with_motion(&owner, lease, request, capture, pacing)
            .map(|result| Reply::Acted {
                effect_updates: Vec::new(),
                receipt: result.receipt,
                after: result.after.map(Into::into),
                after_image: result.after_image.map(Into::into),
                native_lock: result.native_lock,
                effect: result.effect,
            }),
        _ => {
            return Reply::Error {
                code: "InvalidOwner".into(),
            };
        }
    };
    let mut reply = result.unwrap_or_else(error_reply);
    let updates = controller.target_effect_updates();
    match &mut reply {
        Reply::ElementActed { effect_updates, .. } | Reply::Acted { effect_updates, .. } => {
            *effect_updates = updates
        }
        // Keep older target verdicts available on observations, not Home's result.
        Reply::Navigated { .. } => {}
        Reply::ObservedTargets { observation, .. }
        | Reply::ObservedOutline { observation, .. }
        | Reply::Observed { observation }
        | Reply::ObservedNavigation { observation } => observation.effect_updates = updates,
        Reply::ObservedImage { observation } => observation.effect_updates = updates,
        _ => {}
    }
    reply
}

fn error_reply(error: ControllerError) -> Reply {
    match error {
        ControllerError::ViewChanged(changed) => Reply::ViewChanged { changed },
        ControllerError::AdapterAt(diagnostic) => Reply::Error {
            code: diagnostic.code().into(),
        },
        _ => Reply::Error {
            code: format!("{error:?}"),
        },
    }
}

#[cfg(test)]
mod status_tests;

#[cfg(test)]
mod navigation_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_diagnostics_use_flat_fixed_wire_codes() {
        for diagnostic in handkite_core::controller::AdapterDiagnostic::ALL {
            let reply = error_reply(ControllerError::AdapterAt(diagnostic));
            assert_eq!(
                serde_json::to_value(reply).unwrap(),
                serde_json::json!({"type":"error", "code":diagnostic.code()})
            );
        }
    }

    #[test]
    fn act_carries_optional_motion_and_refuses_off_or_malformed() {
        let lease = "1".repeat(32);
        let act = |pacing: &str| {
            format!(
                r#"{{"method":"act","owner":"handkite-mcp","lease":"{lease}","request":{{"device":"phone","expected_revision":1,"action":{{"type":"tap","at":{{"x":1.0,"y":2.0}}}}}}{pacing}}}"#
            )
        };
        let motion = |raw: &str| match serde_json::from_str::<Request>(raw) {
            Ok(Request::Act { pacing, .. }) => Ok(pacing),
            Ok(_) => panic!("not an act request"),
            Err(_) => Err(()),
        };
        assert_eq!(motion(&act("")), Ok(None));
        assert_eq!(
            motion(&act(r#","pacing":{"policy":"cautious","seed":3}"#)),
            Ok(Some(handkite_core::protocol::Motion {
                policy: handkite_core::protocol::MotionPolicy::Cautious,
                seed: 3
            }))
        );
        for bad in [
            r#","pacing":{"policy":"off","seed":3}"#,
            r#","pacing":{"policy":"natural"}"#,
            r#","pacing":{"policy":"natural","seed":3,"extra":1}"#,
            r#","pacing":"natural""#,
        ] {
            assert_eq!(motion(&act(bad)), Err(()), "{bad}");
        }
    }

    #[test]
    fn wire_rejects_invalid_identity_and_unexpected_fields() {
        assert!(
            serde_json::from_str::<Request>(r#"{"method":"observe","device":"../phone"}"#).is_err()
        );
        assert!(
            serde_json::from_str::<Request>(r#"{"method":"observe","device":"phone","other":1}"#)
                .is_err()
        );
    }

    #[test]
    fn wire_requires_a_canonical_opaque_lease_token() {
        for lease in [
            "1",
            "\"1\"",
            "\"0000000000000000000000000000000G\"",
            "\"0000000000000000000000000000000A\"",
        ] {
            let request =
                format!("{{\"method\":\"release\",\"owner\":\"agent\",\"lease\":{lease}}}");
            assert!(serde_json::from_str::<Request>(&request).is_err());
        }
        let request =
            r#"{"method":"release","owner":"agent","lease":"0123456789abcdef0123456789abcdef"}"#;
        assert!(serde_json::from_str::<Request>(request).is_ok());
    }

    #[test]
    fn screenshot_is_base64_not_a_byte_array() {
        let observation = Observation {
            device: DeviceKey::new("phone").unwrap(),
            revision: Revision(1),
            window: ObservationWindow {
                started_unix_ms: 1,
                finished_unix_ms: 2,
            },
            state: DeviceState {
                lock: handkite_core::protocol::LockState::Unlocked,
                wda_ready: true,
                foreground_app: None,
            },
            accessibility_tree: String::new(),
            screenshot: handkite_core::protocol::ScreenImage {
                format: ImageFormat::Png,
                bytes: vec![1, 2, 3],
            },
        };
        let value = serde_json::to_value(WireObservation::from(observation)).unwrap();
        assert_eq!(value["screenshot_base64"], "AQID");
    }

    #[test]
    fn action_reply_keeps_delivery_and_native_lock_proof_separate() {
        for (accepted, native_lock, effect, expected_effect, expected_lock) in [
            (
                None,
                Some(LockState::Locked),
                ObservedEffect::LockConfirmed,
                "lock_confirmed",
                serde_json::json!("locked"),
            ),
            (
                Some(true),
                Some(LockState::Uncertain),
                ObservedEffect::Unconfirmed,
                "unconfirmed",
                serde_json::json!("uncertain"),
            ),
            (
                Some(true),
                Some(LockState::Unlocked),
                ObservedEffect::Unconfirmed,
                "unconfirmed",
                serde_json::json!("unlocked"),
            ),
            (
                Some(true),
                None,
                ObservedEffect::Changed,
                "changed",
                serde_json::Value::Null,
            ),
            (
                Some(false),
                None,
                ObservedEffect::Unconfirmed,
                "unconfirmed",
                serde_json::Value::Null,
            ),
        ] {
            let reply = Reply::Acted {
                effect_updates: Vec::new(),
                receipt: TransportReceipt {
                    accepted,
                    action_kind: if native_lock.is_some() {
                        "lock"
                    } else {
                        "home"
                    }
                    .into(),
                },
                after: None,
                after_image: None,
                native_lock,
                effect,
            };
            let value = serde_json::to_value(reply).unwrap();
            assert_eq!(value["type"], "acted");
            assert_eq!(value["receipt"]["accepted"], serde_json::json!(accepted));
            assert_eq!(value["native_lock"], expected_lock);
            assert_eq!(value["effect"], expected_effect);
            assert!(value["after"].is_null());
        }
    }
}

#[cfg(test)]
mod alert_wire_tests {
    use super::*;
    #[test]
    fn alert_methods_refuse_coordinate_and_label_extensions() {
        assert!(
            serde_json::from_value::<Request>(
                serde_json::json!({"method":"observe_alert","device":"fixture"})
            )
            .is_ok()
        );
        for extra in ["at", "label", "value"] {
            let mut request = serde_json::json!({"method":"tap_alert","owner":"owner","lease":"11111111111111111111111111111111","device":"fixture","expected_revision":1,"alert_id":"a_11111111111111111111111111111111","target_id":"ab_22222222222222222222222222222222"});
            request[extra] = serde_json::json!("forbidden");
            assert!(serde_json::from_value::<Request>(request).is_err());
        }
    }
}
