use super::*;
use std::cell::RefCell;

fn row(id: &str, kind: &str, rect: [f64; 4]) -> Value {
    json!({"ELEMENT":id,"type":kind,"label":"Choose","attribute/name":"choose",
        "attribute/value":null,"attribute/placeholderValue":null,"attribute/focused":false,
        "attribute/selected":false,"enabled":true,"displayed":true,
        "rect":{"x":rect[0],"y":rect[1],"width":rect[2],"height":rect[3]}})
}
fn mapping() -> Mapping {
    Mapping {
        viewport: [0., 0., 4., 8.],
        scale: 1.,
        display_id: Some(1),
    }
}
fn rows() -> Value {
    json!([
        row("app", "XCUIElementTypeApplication", [0., 0., 4., 8.]),
        row("target", "XCUIElementTypeButton", [1., 1., 2., 2.])
    ])
}
fn png(changed: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut data = vec![255; 4 * 8 * 4];
    if changed {
        data[(4 + 1) * 4] = 0;
    }
    {
        let mut encoder = png::Encoder::new(&mut out, 4, 8);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&data)
            .unwrap();
    }
    out
}
fn input(generation: tokio::task::Id) -> Input {
    let rich = rows()[1].clone();
    let a = attrs(&rich).unwrap();
    let region = Pixels::decode(&png(false))
        .unwrap()
        .region(mapping().viewport, &a, &mut 0)
        .unwrap();
    Input {
        token: 1,
        key: cache::Key {
            device: HardwareId::new("fixture").unwrap(),
            session: "session".into(),
            runner: generation,
        },
        app: "example.app".into(),
        action: ElementAction::Tap {
            target_id: "t_0123456789abcdef0123456789abcdef".into(),
        },
        until: Instant::now() + Duration::from_secs(5),
        bound: Bound {
            public: ElementTarget {
                target_id: "t_0123456789abcdef0123456789abcdef".into(),
                source: "xml".into(),
                kind: "XCUIElementTypeButton".into(),
                label: "Choose".into(),
                can_type: false,
                text_only: false,
                rect: [1., 1., 2., 2.],
                value: None,
                value_truncated: false,
            },
            native: "target".into(),
            attrs: a,
            rich,
            region: Some(region),
            focused: Some(false),
            ocr: None,
            ocr_before: None,
            mapping: Some(mapping()),
        },
    }
}
struct Fake {
    rows: Value,
    changed: bool,
    hittable: bool,
    present: bool,
    display: u64,
    app: &'static str,
    final_app_drift: bool,
    final_lock_drift: bool,
    calls: RefCell<Vec<String>>,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            rows: rows(),
            changed: false,
            hittable: true,
            present: true,
            display: 1,
            app: "example.app",
            final_app_drift: false,
            final_lock_drift: false,
            calls: RefCell::new(vec![]),
        }
    }
}
impl WdaRequest for Fake {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        _: Duration,
    ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
        self.calls.borrow_mut().push(path.into());
        let mut status = 200;
        let value = if path.ends_with("/appium/settings") {
            json!({"currentDisplayId":self.display})
        } else if path.ends_with("/wda/activeAppInfo") {
            let final_read = self
                .calls
                .borrow()
                .iter()
                .filter(|p| p.ends_with("/wda/activeAppInfo"))
                .count()
                > 1;
            json!({"bundleId":if final_read && self.final_app_drift { "unexpected.app" } else { self.app }})
        } else if path.ends_with("/wda/locked") {
            let final_read = self
                .calls
                .borrow()
                .iter()
                .filter(|p| p.ends_with("/wda/locked"))
                .count()
                > 1;
            json!(final_read && self.final_lock_drift)
        } else if path == "/screenshot" {
            json!(base64::engine::general_purpose::STANDARD.encode(png(self.changed)))
        } else if path.ends_with("/elements") {
            assert_eq!(method, "POST");
            let p = serde_json::from_slice::<Value>(body).unwrap()["value"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(
                p.contains("XCUIElementTypeApplication")
                    && p.contains("XCUIElementTypeSecureTextField")
            );
            assert!(!p.contains("enabled ==") && !p.contains("visible =="));
            self.rows.clone()
        } else if path.ends_with("/attribute/hittable") {
            json!(self.hittable)
        } else if path.ends_with("/attribute/type") {
            if self.present {
                json!("XCUIElementTypeButton")
            } else {
                status = 404;
                json!({"error":"stale element reference"})
            }
        } else {
            panic!("unexpected request {method} {path}")
        };
        Ok(HttpResponse {
            status,
            body: serde_json::to_vec(&json!({"sessionId":"session","value":value})).unwrap(),
        })
    }
}

#[tokio::test]
async fn fresh_region_and_identity_need_no_source_or_input() {
    let task = tokio::spawn(async {});
    let f = Fake::default();
    revalidate(&f, &input(task.id())).await.unwrap();
    assert_eq!(
        f.calls
            .borrow()
            .iter()
            .filter(|p| p.as_str() == "/screenshot")
            .count(),
        1
    );
    assert!(
        !f.calls
            .borrow()
            .iter()
            .any(|p| p.contains("source") || p.ends_with("click"))
    );
}

#[tokio::test]
async fn stale_moved_hidden_ambiguous_or_changed_region_refuses() {
    let task = tokio::spawn(async {});
    for case in 0..9 {
        let mut f = Fake::default();
        match case {
            0 => f.changed = true,
            1 => f.rows[1]["rect"]["x"] = json!(2),
            2 => f.rows[1]["enabled"] = json!(false),
            3 => f.rows[1]["displayed"] = json!(false),
            4 => {
                let mut r = f.rows[1].clone();
                r["ELEMENT"] = json!("duplicate");
                f.rows.as_array_mut().unwrap().push(r);
            }
            5 => f.rows[1]["ELEMENT"] = json!("remapped"),
            6 => f.display = 2,
            7 => f.app = "unexpected.app",
            _ => f.hittable = false,
        }
        assert!(
            revalidate(&f, &input(task.id())).await.is_err(),
            "case {case}"
        );
    }
}

#[test]
fn blockers_and_malformed_context_are_never_filtered_away() {
    for kind in [
        "XCUIElementTypeAlert",
        "XCUIElementTypeSheet",
        "XCUIElementTypeSecureTextField",
    ] {
        let mut r = rows();
        let mut hidden = row("blocker", kind, [0., 0., 1., 1.]);
        hidden["displayed"] = json!(false);
        r.as_array_mut().unwrap().push(hidden);
        assert!(checked_rows(&r, mapping()).is_err());
    }
    for bad in [
        "proud-lock",
        "proud-\u{1}lock",
        "proud-\nlock",
        "Enter Passcode",
    ] {
        let mut r = rows();
        r[0]["attribute/name"] = json!(bad);
        assert!(checked_rows(&r, mapping()).is_err());
    }
    let mut multiline = rows();
    multiline[1]["attribute/value"] = json!("ordinary\nmultiline text");
    assert!(checked_rows(&multiline, mapping()).is_ok());
    for r in [
        json!(null),
        json!({}),
        json!([]),
        json!([rows()[1]]),
        json!([rows()[0], rows()[0]]),
    ] {
        assert!(checked_rows(&r, mapping()).is_err());
    }
    let mut r = rows();
    r[0]["rect"]["x"] = json!(1);
    assert!(checked_rows(&r, mapping()).is_err());
}

#[tokio::test]
async fn post_capture_does_not_claim_renames_remaps_or_unproven_absence() {
    let task = tokio::spawn(async {});
    let input = input(task.id());
    for case in 0..4 {
        let mut f = Fake {
            changed: true,
            ..Fake::default()
        };
        match case {
            0 => f.rows[1]["label"] = json!("Renamed"),
            1 => f.rows[1]["ELEMENT"] = json!("remapped"),
            2 => {
                f.rows.as_array_mut().unwrap().pop();
            }
            _ => {
                f.rows.as_array_mut().unwrap().pop();
                f.present = false;
            }
        }
        let (raw, next, verified, _) = after(&f, &input).await.unwrap();
        assert_eq!(verified, case == 3);
        assert!(raw.accessibility_tree.is_empty());
        assert!(next.is_empty());
    }
}

#[tokio::test]
async fn post_capture_renews_only_unchanged_tap_binding_without_text_authority() {
    let task = tokio::spawn(async {});
    let input = input(task.id());
    let f = Fake::default();
    let (_, next, verified, _) = after(&f, &input).await.unwrap();
    assert!(!verified);
    assert_eq!(next.len(), 1);
    assert_ne!(next[0].public.target_id, input.bound.public.target_id);
    assert!(!next[0].public.can_type && !next[0].public.text_only);
    let mut changed = Fake::default();
    changed.rows[1]["attribute/selected"] = json!(true);
    let (_, next, verified, _) = after(&changed, &input).await.unwrap();
    assert!(verified);
    assert!(next.is_empty());
}

#[tokio::test]
async fn post_capture_fences_app_and_lock_after_all_evidence() {
    let task = tokio::spawn(async {});
    for absent_target in [false, true] {
        for app_drift in [false, true] {
            let mut f = Fake {
                final_app_drift: app_drift,
                final_lock_drift: !app_drift,
                changed: true,
                ..Fake::default()
            };
            if absent_target {
                f.rows.as_array_mut().unwrap().pop();
                f.present = false;
            }
            assert!(after(&f, &input(task.id())).await.is_err());
            if absent_target {
                let calls = f.calls.borrow();
                assert!(
                    calls
                        .iter()
                        .position(|p| p.ends_with("/attribute/type"))
                        .unwrap()
                        < calls
                            .iter()
                            .rposition(|p| p.ends_with("/wda/locked"))
                            .unwrap()
                );
            }
        }
    }
}

#[tokio::test]
async fn equivalent_boolean_encodings_never_prove_an_effect() {
    let task = tokio::spawn(async {});
    for key in ["enabled", "displayed", "attribute/selected"] {
        let mut f = Fake::default();
        f.rows[1][key] = json!(f.rows[1][key].as_bool().unwrap().to_string());
        let (_, next, verified, _) = after(&f, &input(task.id())).await.unwrap();
        assert!(!verified);
        assert!(next.is_empty());
    }
}

#[tokio::test]
async fn empty_and_null_labels_query_the_complete_type_cohort() {
    let task = tokio::spawn(async {});
    let mut input = input(task.id());
    for label in [Value::Null, json!("")] {
        input.bound.rich["label"] = label;
        let p = predicate(&input.bound).unwrap();
        assert!(p.contains("type == \"XCUIElementTypeButton\""));
        assert!(!p.contains("AND label"));
        assert!(p.contains("MATCHES[c]"));
        assert!(p.contains(r"\\p{Cc}"));
    }
}

#[tokio::test]
async fn native_read_starts_only_after_target_region_validation() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let task = tokio::spawn(async {});
    let mut f = Fake::default();
    f.changed = true;
    let called = Arc::new(AtomicBool::new(false));
    let c = called.clone();
    assert!(
        revalidate_with_native(
            &f,
            &input(task.id()),
            Instant::now() + Duration::from_secs(5),
            move |_| {
                c.store(true, Ordering::SeqCst);
                LockState::Unlocked
            }
        )
        .await
        .is_err()
    );
    assert!(!called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn native_failure_or_tail_failure_drains_the_read_without_input() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let task = tokio::spawn(async {});
    for lock in [LockState::Locked, LockState::Uncertain, LockState::Unlocked] {
        let mut f = Fake::default();
        if lock == LockState::Unlocked {
            f.hittable = false;
        }
        let finished = Arc::new(AtomicBool::new(false));
        let end = finished.clone();
        assert!(
            revalidate_with_native(
                &f,
                &input(task.id()),
                Instant::now() + Duration::from_secs(5),
                move |_| {
                    std::thread::sleep(Duration::from_millis(10));
                    end.store(true, Ordering::SeqCst);
                    lock
                }
            )
            .await
            .is_err()
        );
        assert!(finished.load(Ordering::SeqCst));
        assert!(f.calls.borrow().iter().all(|p| !p.ends_with("/click")));
    }
}

#[tokio::test]
async fn successful_native_proof_caps_the_socket_write_age() {
    let task = tokio::spawn(async {});
    let f = Fake::default();
    let proof = revalidate_with_native(
        &f,
        &input(task.id()),
        Instant::now() + Duration::from_secs(5),
        move |_| LockState::Unlocked,
    )
    .await
    .unwrap();
    assert_eq!(
        proof.deadline.duration_since(proof.started),
        Duration::from_millis(500)
    );
    assert!(Instant::now() < proof.deadline);
    let original = Instant::now() + Duration::from_millis(100);
    let short = revalidate_with_native(&f, &input(task.id()), original, |_| LockState::Unlocked)
        .await
        .unwrap();
    assert_eq!(short.write_deadline().unwrap(), original);
    let past = Instant::now() - Duration::from_millis(1);
    assert!(
        revalidate_with_native(&f, &input(task.id()), past, move |_| panic!(
            "expired input must not query native state"
        ))
        .await
        .is_err()
    );
}

#[tokio::test]
async fn failed_native_read_still_restores_profile_and_preserves_restore_failure() {
    use std::cell::Cell;
    struct Profile {
        inner: Fake,
        settings: RefCell<Value>,
        restored: Cell<bool>,
        fail_restore: bool,
        restore_delay: Duration,
    }
    impl WdaRequest for Profile {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            timeout: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            if !path.ends_with("/appium/settings") {
                return self.inner.request(method, path, body, timeout).await;
            }
            if method == "POST" {
                let new: Value = serde_json::from_slice(body).unwrap();
                if new["settings"]["shouldUseCompactResponses"] == true {
                    self.restored.set(true);
                    if self.fail_restore {
                        return Ok(HttpResponse {
                            status: 500,
                            body: br#"{}"#.to_vec(),
                        });
                    }
                    tokio::time::sleep(self.restore_delay).await;
                }
                for (key, value) in new["settings"].as_object().unwrap() {
                    self.settings.borrow_mut()[key] = value.clone();
                }
            }
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(
                    &json!({"sessionId":"session", "value":self.settings.borrow().clone()}),
                )
                .unwrap(),
            })
        }
    }
    let task = tokio::spawn(async {});
    for lock in [LockState::Locked, LockState::Uncertain, LockState::Unlocked] {
        let f = Profile {
            inner: Fake::default(),
            settings: RefCell::new(
                json!({"currentDisplayId":1,"shouldUseCompactResponses":true,"elementResponseAttributes":"id"}),
            ),
            restored: Cell::new(false),
            fail_restore: lock != LockState::Unlocked,
            restore_delay: Duration::from_millis(550),
        };
        let input = input(task.id());
        let result = rich_context(
            &f,
            "session",
            ATTRS,
            revalidate_with_native(
                &f,
                &input,
                Instant::now() + Duration::from_secs(5),
                move |_| lock,
            ),
        )
        .await;
        assert!(f.restored.get());
        if lock != LockState::Unlocked {
            assert!(matches!(result, Err(_)));
        } else {
            let proof = result.unwrap().unwrap();
            assert!(proof.write_deadline().is_err());
        }
        assert!(
            f.inner
                .calls
                .borrow()
                .iter()
                .all(|p| !p.ends_with("/click"))
        );
    }
}
