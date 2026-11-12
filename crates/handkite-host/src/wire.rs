use base64::Engine;
use handkite_core::controller::{Controller, ControllerError, DeviceAdapter, LeaseId, ViewChange};
use handkite_core::protocol::{
    ActionRequest, DeviceKey, DeviceState, ImageFormat, LockState, Observation, ObservationWindow,
    ObservedEffect, Revision, TransportReceipt,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Observe {
        device: DeviceKey,
    },
    Acquire {
        owner: String,
        ttl_ms: u64,
    },
    Release {
        owner: String,
        lease: LeaseId,
    },
    Act {
        owner: String,
        lease: LeaseId,
        request: ActionRequest,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Observed {
        observation: WireObservation,
    },
    Acquired {
        lease: LeaseId,
    },
    Released,
    Acted {
        receipt: TransportReceipt,
        after: Option<WireObservation>,
        native_lock: Option<LockState>,
        effect: ObservedEffect,
    },
    Error {
        code: String,
    },
    ViewChanged {
        changed: ViewChange,
    },
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
}

impl From<Observation> for WireObservation {
    fn from(value: Observation) -> Self {
        Self {
            device: value.device,
            revision: value.revision,
            window: value.window,
            state: value.state,
            accessibility_tree: value.accessibility_tree,
            screenshot_format: value.screenshot.format,
            screenshot_base64: base64::engine::general_purpose::STANDARD
                .encode(value.screenshot.bytes),
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
    let result = match request {
        Request::Observe { device } => {
            controller
                .observe(&device)
                .map(|observation| Reply::Observed {
                    observation: observation.into(),
                })
        }
        Request::Acquire { owner, ttl_ms } if valid_owner(&owner) => controller
            .acquire(owner, Duration::from_millis(ttl_ms))
            .map(|lease| Reply::Acquired { lease }),
        Request::Release { owner, lease } if valid_owner(&owner) => {
            controller.release(&owner, lease).map(|()| Reply::Released)
        }
        Request::Act {
            owner,
            lease,
            request,
        } if valid_owner(&owner) => {
            controller
                .act(&owner, lease, request)
                .map(|result| Reply::Acted {
                    receipt: result.receipt,
                    after: result.after.map(Into::into),
                    native_lock: result.native_lock,
                    effect: result.effect,
                })
        }
        _ => {
            return Reply::Error {
                code: "InvalidOwner".into(),
            };
        }
    };
    result.unwrap_or_else(|error| match error {
        ControllerError::ViewChanged(changed) => Reply::ViewChanged { changed },
        _ => Reply::Error {
            code: format!("{error:?}"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
