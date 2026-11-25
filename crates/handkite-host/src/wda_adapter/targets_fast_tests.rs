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
pub(in crate::wda_adapter) fn input(generation: tokio::task::Id) -> Input {
    let rich = rows()[1].clone();
    let a = attrs(&rich).unwrap();
    let region = Pixels::decode(&png(false))
        .unwrap()
        .region(mapping().viewport, &a, &mut 0)
        .unwrap();
    Input {
        token: 1,
        text_basis: None,
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
    frame: Option<Vec<u8>>,
    frames: RefCell<std::collections::VecDeque<Vec<u8>>>,
    calls: RefCell<Vec<String>>,
    settings: RefCell<Value>,
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
            frame: None,
            frames: RefCell::new(Default::default()),
            calls: RefCell::new(vec![]),
            settings: RefCell::new(
                json!({"shouldUseCompactResponses":true,"elementResponseAttributes":"id"}),
            ),
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
            if method == "POST" {
                let settings: Value = serde_json::from_slice(body).unwrap();
                for (key, value) in settings["settings"].as_object().unwrap() {
                    self.settings.borrow_mut()[key] = value.clone();
                }
            }
            let mut settings = self.settings.borrow().clone();
            settings["currentDisplayId"] = json!(self.display);
            settings
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
            json!(
                base64::engine::general_purpose::STANDARD.encode(
                    self.frames
                        .borrow_mut()
                        .pop_front()
                        .or_else(|| self.frame.clone())
                        .unwrap_or_else(|| png(self.changed))
                )
            )
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
        2
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
        .unwrap()
        .is_err()
    );
    assert!(!called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn native_failure_drains_and_bad_tail_never_queries() {
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
            .unwrap()
            .is_err()
        );
        assert_eq!(finished.load(Ordering::SeqCst), lock != LockState::Unlocked);
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
    .unwrap()
    .unwrap();
    assert_eq!(
        proof.deadline.duration_since(proof.started),
        Duration::from_millis(500)
    );
    assert!(Instant::now() < proof.deadline);
    let original = Instant::now() + Duration::from_millis(100);
    let short = revalidate_with_native(&f, &input(task.id()), original, |_| LockState::Unlocked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(short.write_deadline().unwrap(), original);
    let past = Instant::now() - Duration::from_millis(1);
    assert!(
        revalidate_with_native(&f, &input(task.id()), past, move |_| panic!(
            "expired input must not query native state"
        ))
        .await
        .unwrap()
        .is_err()
    );
}

#[tokio::test]
async fn restored_settings_precede_native_proof_and_final_drift_still_refuses() {
    use std::cell::Cell;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct Profile {
        inner: Fake,
        settings: RefCell<Value>,
        restored: Cell<bool>,
        fail_restore: bool,
        restore_delay: Duration,
        restore_finished: Arc<AtomicBool>,
        drift: u8,
    }
    impl WdaRequest for Profile {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            timeout: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            if self.restore_finished.load(Ordering::SeqCst)
                && ((self.drift == 1 && path.ends_with("/wda/activeAppInfo"))
                    || (self.drift == 2 && path.ends_with("/wda/locked")))
            {
                let value = if self.drift == 1 {
                    json!({"bundleId":"unexpected.app"})
                } else {
                    json!(true)
                };
                return Ok(HttpResponse {
                    status: 200,
                    body: serde_json::to_vec(&json!({"sessionId":"session", "value":value}))
                        .unwrap(),
                });
            }
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
                    self.restore_finished.store(true, Ordering::SeqCst);
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
    for (lock, fail_restore, deadline_ms, drift) in [
        (LockState::Unlocked, false, 5000, 0),
        (LockState::Locked, false, 5000, 0),
        (LockState::Uncertain, false, 5000, 0),
        (LockState::Unlocked, true, 5000, 0),
        (LockState::Unlocked, false, 100, 0),
        (LockState::Unlocked, false, 5000, 1),
        (LockState::Unlocked, false, 5000, 2),
    ] {
        let f = Profile {
            inner: Fake::default(),
            settings: RefCell::new(
                json!({"currentDisplayId":1,"shouldUseCompactResponses":true,"elementResponseAttributes":"id"}),
            ),
            restored: Cell::new(false),
            fail_restore,
            restore_delay: Duration::from_millis(550),
            restore_finished: Arc::new(AtomicBool::new(false)),
            drift,
        };
        let input = input(task.id());
        let restored = f.restore_finished.clone();
        let called = Arc::new(AtomicBool::new(false));
        let native_called = called.clone();
        let result = revalidate_with_native(
            &f,
            &input,
            Instant::now() + Duration::from_millis(deadline_ms),
            move |_| {
                assert!(restored.load(Ordering::SeqCst));
                std::thread::sleep(Duration::from_millis(10));
                native_called.store(true, Ordering::SeqCst);
                lock
            },
        )
        .await;
        assert!(f.restored.get());
        if fail_restore {
            assert!(matches!(result, Err(_)));
            assert!(!called.load(Ordering::SeqCst));
        } else if deadline_ms == 100 {
            assert!(result.unwrap().is_err());
            assert!(!called.load(Ordering::SeqCst));
        } else if lock != LockState::Unlocked || drift != 0 {
            assert!(result.unwrap().is_err());
            assert!(called.load(Ordering::SeqCst));
        } else {
            let proof = result.unwrap().unwrap();
            assert!(called.load(Ordering::SeqCst));
            assert!(proof.write_deadline().is_ok());
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

#[tokio::test]
async fn late_native_proof_is_drained_without_extending_the_write_deadline() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let task = tokio::spawn(async {});
    let f = Fake::default();
    let completed = Arc::new(AtomicBool::new(false));
    let done = completed.clone();
    let result = revalidate_with_native(
        &f,
        &input(task.id()),
        Instant::now() + Duration::from_secs(5),
        move |_| {
            std::thread::sleep(Duration::from_millis(550));
            done.store(true, Ordering::SeqCst);
            LockState::Unlocked
        },
    )
    .await;
    assert!(result.unwrap().is_err());
    assert!(completed.load(Ordering::SeqCst));
    assert!(f.calls.borrow().iter().all(|p| !p.ends_with("/click")));
}

fn png_pixel(x: usize, y: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut data = vec![255; 4 * 8 * 4];
    data[(y * 4 + x) * 4] = 0;
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

#[tokio::test]
async fn outside_animation_admits_but_inside_and_margin_pixels_refuse() {
    let task = tokio::spawn(async {});
    for (x, y, admitted) in [(3, 7, true), (1, 1, false), (0, 1, false)] {
        let f = Fake {
            frames: RefCell::new([png(false), png_pixel(x, y)].into()),
            ..Fake::default()
        };
        assert_eq!(revalidate(&f, &input(task.id())).await.is_ok(), admitted);
        assert!(
            !f.calls
                .borrow()
                .iter()
                .any(|p| p.ends_with("/click") || p.ends_with("/actions"))
        );
        assert!(f.calls.borrow().iter().any(|p| p.ends_with("/elements")));
        assert!(
            !f.calls
                .borrow()
                .iter()
                .any(|p| p.contains("source") || p.ends_with("/click"))
        );
    }
    let transient = Fake {
        frames: RefCell::new([png_pixel(1, 1), png(false)].into()),
        ..Fake::default()
    };
    assert!(revalidate(&transient, &input(task.id())).await.is_err());
    assert!(
        !transient
            .calls
            .borrow()
            .iter()
            .any(|p| p.ends_with("/click") || p.ends_with("/actions"))
    );
}

#[tokio::test]
async fn unchanged_pixels_never_bypass_fresh_native_identity_and_attributes() {
    let task = tokio::spawn(async {});
    for case in 0..7 {
        let mut f = Fake::default();
        match case {
            0 => f.rows[1]["ELEMENT"] = json!("replacement"),
            1 => f.rows[1]["rect"]["x"] = json!(1.25),
            2 => f.rows[1]["attribute/value"] = json!("changed"),
            3 => f.rows[1]["attribute/selected"] = json!(true),
            4 => f.rows[1]["enabled"] = json!(false),
            5 => f.rows[1]["displayed"] = json!(false),
            _ => {
                let duplicate = f.rows[1].clone();
                f.rows.as_array_mut().unwrap().push(duplicate);
            }
        }
        assert!(revalidate(&f, &input(task.id())).await.is_err());
        assert!(f.calls.borrow().iter().any(|p| p.ends_with("/elements")));
        assert!(
            !f.calls
                .borrow()
                .iter()
                .any(|p| p.ends_with("/click") || p.ends_with("/actions"))
        );
    }
}

#[test]
fn delta_sample_classification_matches_the_standalone_effect_rules() {
    let task_id = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { tokio::spawn(async {}).id() });
    let bound = input(task_id).bound;
    let same = Pixels::decode(&png(false))
        .unwrap()
        .region(mapping().viewport, &bound.attrs, &mut 0)
        .unwrap();
    let moved = Pixels::decode(&png(true))
        .unwrap()
        .region(mapping().viewport, &bound.attrs, &mut 0)
        .unwrap();
    let app = rows()[0].clone();
    let present = rows().as_array().unwrap().clone();
    assert_eq!(
        classify(&bound, &present, &moved, None),
        Outcome::Unconfirmed
    );
    let mut toggled = present.clone();
    toggled[1]["attribute/selected"] = json!(true);
    assert_eq!(classify(&bound, &toggled, &same, None), Outcome::Changed);
    let gone = vec![app.clone()];
    assert_eq!(classify(&bound, &gone, &moved, None), Outcome::Absent);
    assert_eq!(classify(&bound, &gone, &same, None), Outcome::Unconfirmed);
    let duplicate = vec![app, row("other", "XCUIElementTypeButton", [1., 1., 2., 2.])];
    assert_eq!(
        classify(&bound, &duplicate, &moved, None),
        Outcome::Unconfirmed
    );
}

// Same object, not a replacement ID that WDA's stale-element check can reject.
async fn same_live_id_attribute_drift_must_refuse(attribute: &str, changed: Value) {
    let task = tokio::spawn(async {});
    let bound = input(task.id());
    let mut f = Fake::default();
    f.rows[1][attribute] = changed;
    assert_eq!(f.rows[1]["ELEMENT"], bound.bound.native);
    assert_ne!(f.rows[1], bound.bound.rich);
    let result =
        revalidate_with_native(&f, &bound, Instant::now() + Duration::from_secs(5), |_| {
            LockState::Unlocked
        })
        .await;
    assert!(result.unwrap().is_err());
    let calls = f.calls.borrow();
    assert!(calls.iter().any(|p| p.ends_with("/elements")));
    assert!(
        !calls
            .iter()
            .any(|p| p.ends_with("/click") || p.ends_with("/actions"))
    );
}

#[tokio::test]
async fn same_live_id_value_drift_must_refuse() {
    same_live_id_attribute_drift_must_refuse("attribute/value", json!("changed")).await;
}

#[tokio::test]
async fn same_live_id_selected_drift_must_refuse() {
    same_live_id_attribute_drift_must_refuse("attribute/selected", json!(true)).await;
}

#[tokio::test]
async fn overlapping_validation_keeps_image_brackets_and_drains_read_failures() {
    use std::cell::Cell;
    struct Ordered {
        inner: Fake,
        fail_items: bool,
        images: Cell<usize>,
        images_done: Cell<usize>,
        items_done: Cell<bool>,
        apps_started: Cell<usize>,
        apps_done: Cell<usize>,
        display_after_started: Cell<bool>,
        display_after_done: Cell<bool>,
    }
    impl WdaRequest for Ordered {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: &[u8],
            timeout: Duration,
        ) -> Result<HttpResponse, crate::wda_transport::WdaError> {
            if path == "/screenshot" {
                let index = self.images.get() + 1;
                self.images.set(index);
                if index == 2 {
                    assert!(
                        self.items_done.get(),
                        "final image preceded native response"
                    );
                }
                tokio::task::yield_now().await;
                let result = self.inner.request(method, path, body, timeout).await;
                self.images_done.set(index);
                return result;
            }
            if path.ends_with("/elements") {
                assert_eq!(
                    self.images_done.get(),
                    1,
                    "native read preceded first image"
                );
                tokio::task::yield_now().await;
                self.items_done.set(true);
                if self.fail_items {
                    return Err(crate::wda_transport::WdaError::NotConnected);
                }
            }
            if path.ends_with("/wda/activeAppInfo") {
                self.apps_started.set(self.apps_started.get() + 1);
                tokio::time::sleep(Duration::from_millis(5)).await;
                self.apps_done.set(self.apps_done.get() + 1);
            }
            if path.ends_with("/appium/settings") && method == "GET" && self.images_done.get() == 2
            {
                if self.display_after_started.replace(true) {
                    // A restoration read must not mask cancellation of the
                    // display read joined with final image validation.
                    assert!(self.display_after_done.get());
                } else {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    self.display_after_done.set(true);
                }
            }
            if path.ends_with("/appium/settings") && method == "POST" {
                let settings: Value = serde_json::from_slice(body).unwrap();
                if settings["settings"]["elementResponseAttributes"] == "id" {
                    assert_eq!(self.apps_started.get(), self.apps_done.get());
                    assert!(self.items_done.get());
                    assert_eq!(self.images.get(), self.images_done.get());
                }
            }
            self.inner.request(method, path, body, timeout).await
        }
    }
    for fault in 0..4 {
        let f = Ordered {
            inner: Fake {
                frames: RefCell::new(
                    [
                        if fault == 1 { vec![] } else { png(false) },
                        if fault == 3 { vec![] } else { png(false) },
                    ]
                    .into(),
                ),
                ..Fake::default()
            },
            fail_items: fault == 2,
            images: Cell::new(0),
            images_done: Cell::new(0),
            items_done: Cell::new(false),
            apps_started: Cell::new(0),
            apps_done: Cell::new(0),
            display_after_started: Cell::new(false),
            display_after_done: Cell::new(false),
        };
        let task = tokio::spawn(async {});
        let result = revalidate_with_native(
            &f,
            &input(task.id()),
            Instant::now() + Duration::from_secs(5),
            |_| LockState::Unlocked,
        )
        .await
        .unwrap();
        assert_eq!(result.is_ok(), fault == 0, "fault {fault}");
        assert!(f.items_done.get());
        assert_eq!(f.apps_started.get(), f.apps_done.get());
        assert_eq!(f.images.get(), f.images_done.get());
        if fault != 1 {
            assert_eq!(f.images_done.get(), 2);
        }
        if fault == 0 || fault == 3 {
            assert!(f.display_after_done.get());
        }
        assert_eq!(f.inner.settings.borrow()["elementResponseAttributes"], "id");
        assert!(
            !f.inner
                .calls
                .borrow()
                .iter()
                .any(|p| p.ends_with("/click") || p.ends_with("/actions"))
        );
    }
}
