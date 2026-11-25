use super::{
    catalog::Tool,
    response::{self, exchange_reply},
    session::Session,
    tool_error,
};
use crate::wire::Reply;
use handkite_core::{
    controller::AlertRequest,
    protocol::{DeviceKey, LockState, ObservedEffect, Revision},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tap {
    expected_revision: Revision,
    alert_id: String,
    target_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
fn parse(raw: &str) -> Result<Tap, ()> {
    let p: Tap = serde_json::from_str(raw).map_err(|_| ())?;
    if !(AlertRequest {
        alert_id: p.alert_id.clone(),
        target_id: p.target_id.clone(),
    })
    .valid()
    {
        return Err(());
    }
    Ok(p)
}
pub(super) fn valid_raw(tool: Tool, raw: &str) -> bool {
    if tool == Tool::TapAlert {
        parse(raw).is_ok()
    } else {
        serde_json::from_str::<Empty>(raw).is_ok()
    }
}
pub(super) fn schema(tool: Tool) -> Value {
    if tool == Tool::ObserveAlert {
        return json!({"type":"object","additionalProperties":false,"properties":{}});
    }
    json!({"type":"object","additionalProperties":false,"properties":{"expected_revision":{"type":"integer","minimum":0},"alert_id":{"type":"string","pattern":"^a_[0-9a-f]{32}$"},"target_id":{"type":"string","pattern":"^ab_[0-9a-f]{32}$"}},"required":["expected_revision","alert_id","target_id"]})
}
pub(super) fn call<F: FnMut(&[u8]) -> io::Result<Vec<u8>>>(
    tool: Tool,
    args: Value,
    device: &DeviceKey,
    session: &mut Session,
    exchange: &mut F,
) -> Value {
    session.presentation.clear();
    session.target_inventory = None;
    session.home_observe_hint = false;
    if tool == Tool::ObserveAlert {
        if !valid_raw(tool, &args.to_string()) {
            return tool_error("invalid alert arguments");
        }
        return match exchange_reply(json!({"method":"observe_alert","device":device}), exchange) {
            Ok(Reply::ObservedAlert { status, alert })
                if &status.device == device
                    && status.state.lock == LockState::Unlocked
                    && status.state.wda_ready
                    && status.state.foreground_app.as_deref() == Some("com.apple.springboard")
                    && alert.valid() =>
            {
                response::text_result(
                    json!({"revision":status.revision,"window":status.window,"state":status.state,"revision_scope":"alert","allowed_actions":["tap_alert"],"alert":alert,"requester_identity":"unproven"}),
                )
            }
            Ok(Reply::Error { code }) => {
                response::with_service_code(tool_error("alert unavailable"), &code)
            }
            _ => tool_error("alert observation unavailable"),
        };
    }
    let Some(lease) = session.lease else {
        return tool_error("explicit control acquisition required");
    };
    let Ok(p) = parse(&args.to_string()) else {
        return tool_error("invalid alert arguments");
    };
    match exchange_reply(
        json!({"method":"tap_alert","owner":"handkite-mcp","lease":lease,"device":device,"expected_revision":p.expected_revision,"alert_id":p.alert_id,"target_id":p.target_id}),
        exchange,
    ) {
        Ok(Reply::AlertActed { receipt, effect })
            if receipt.action_kind == "tap_alert"
                && matches!(
                    effect,
                    ObservedEffect::Changed | ObservedEffect::Unconfirmed
                ) =>
        {
            let mut r = response::text_result(
                json!({"receipt":receipt,"effect":effect,"revision_scope":"none","allowed_actions":[],"permission_state":"not_verified"}),
            );
            if receipt.accepted != Some(true) || effect != ObservedEffect::Changed {
                r["isError"] = json!(true)
            }
            r
        }
        Ok(Reply::Error { code }) => {
            if code == "InvalidLease" {
                session.lease = None
            }
            let pre = matches!(
                code.as_str(),
                "AdapterAlertRevalidation"
                    | "AdapterNativeBefore"
                    | "StaleRevision"
                    | "StaleObservation"
                    | "InvalidObservationWindow"
                    | "InvalidLease"
                    | "DeviceMismatch"
                    | "LockedAction"
            );
            response::with_service_code(uncertain(pre), &code)
        }
        _ => uncertain(false),
    }
}
fn uncertain(pre: bool) -> Value {
    let mut r = response::text_result(
        json!({"receipt":{"action_kind":"tap_alert","accepted":if pre{Some(false)}else{None}},"effect":"unconfirmed","delivery":if pre{"not_sent"}else{"unknown"},"revision_scope":"none","allowed_actions":[]}),
    );
    r["isError"] = json!(true);
    r
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alert_handles_cannot_be_coordinates_labels_or_ordinary_ids() {
        let good = r#"{"expected_revision":1,"alert_id":"a_11111111111111111111111111111111","target_id":"ab_22222222222222222222222222222222"}"#;
        assert!(valid_raw(Tool::TapAlert, good));
        for bad in [
            good.replace("ab_", "t_"),
            good.replace(
                "\"expected_revision\":1",
                "\"expected_revision\":1,\"expected_revision\":2",
            ),
            good.replace("\"expected_revision\":1", "\"expected_revision\":1,\"x\":4"),
            good.replace("a_111", "a_A11"),
        ] {
            assert!(!valid_raw(Tool::TapAlert, &bad))
        }
    }
    #[test]
    fn unauthorized_tap_never_contacts_backend() {
        let mut s = Session::default();
        let mut calls = 0;
        let r = call(
            Tool::TapAlert,
            json!({}),
            &DeviceKey::new("fixture").unwrap(),
            &mut s,
            &mut |_| {
                calls += 1;
                unreachable!()
            },
        );
        assert_eq!(calls, 0);
        assert_eq!(r["isError"], true);
    }
    #[test]
    fn unknown_transport_is_never_reported_not_sent() {
        let mut s = Session::default();
        s.lease = Some(
            "11111111111111111111111111111111"
                .to_owned()
                .try_into()
                .unwrap(),
        );
        let r = call(
            Tool::TapAlert,
            json!({"expected_revision":1,"alert_id":"a_11111111111111111111111111111111","target_id":"ab_22222222222222222222222222222222"}),
            &DeviceKey::new("fixture").unwrap(),
            &mut s,
            &mut |_| Err(io::Error::other("fixture")),
        );
        assert!(r.to_string().contains("unknown"));
    }
    #[test]
    fn observe_alert_is_one_metadata_only_request_and_never_navigation_fallback() {
        let device = DeviceKey::new("fixture").unwrap();
        let mut session = Session::default();
        session.home_observe_hint = true;
        let mut calls = 0;
        let r = call(
            Tool::ObserveAlert,
            json!({}),
            &device,
            &mut session,
            &mut |req| {
                calls += 1;
                assert_eq!(
                    serde_json::from_slice::<Value>(req).unwrap()["method"],
                    "observe_alert"
                );
                Ok(
                    serde_json::to_vec(&json!({"type":"error","code":"AdapterAlertCapture"}))
                        .unwrap(),
                )
            },
        );
        assert_eq!(calls, 1);
        assert_eq!(r["isError"], true);
        assert!(!session.home_observe_hint);
        assert!(!r.to_string().contains("screenshot"));
    }
}
