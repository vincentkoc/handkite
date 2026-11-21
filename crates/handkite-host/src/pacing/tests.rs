use super::*;

const POLICIES: [Policy; 2] = [Policy::Natural, Policy::Cautious];

fn shape(policy: Policy, seed: u64) -> Shape {
    Shape {
        policy: policy.motion().unwrap(),
        seed,
    }
}

fn tap(x: f64, y: f64) -> Action {
    Action::Tap { at: Point { x, y } }
}

#[test]
fn policy_names_are_closed_and_default_off() {
    assert_eq!(Policy::default(), Policy::Off);
    for (name, policy) in [
        ("off", Policy::Off),
        ("natural", Policy::Natural),
        ("cautious", Policy::Cautious),
    ] {
        assert_eq!(Policy::parse(name), Some(policy));
        assert_eq!(serde_json::to_value(policy).unwrap(), json!(name));
    }
    for name in ["", "Natural", "fast", "human"] {
        assert_eq!(Policy::parse(name), None);
    }
    assert!(Policy::Off.profile().is_none());
}

#[test]
fn override_is_optional_and_strict() {
    let mut arguments = json!({"expected_revision":1});
    assert_eq!(Override::take(&mut arguments), Ok(None));
    let mut arguments = json!({"pacing":{"policy":"cautious","seed":9}});
    assert_eq!(
        Override::take(&mut arguments),
        Ok(Some(Override {
            policy: Some(Policy::Cautious),
            seed: Some(9)
        }))
    );
    assert_eq!(arguments, json!({}));
    for bad in [
        json!({"pacing":{"policy":"fast"}}),
        json!({"pacing":{"jitter":1}}),
        json!({"pacing":"natural"}),
        json!({"pacing":{"seed":-1}}),
    ] {
        let mut bad = bad;
        assert!(Override::take(&mut bad).is_err());
    }
}

#[test]
fn rng_is_seed_deterministic_and_unit_bounded() {
    let mut a = Rng::new(42);
    let mut b = Rng::new(42);
    let mut c = Rng::new(43);
    let first: Vec<u64> = (0..64).map(|_| a.next_u64()).collect();
    let second: Vec<u64> = (0..64).map(|_| b.next_u64()).collect();
    let third: Vec<u64> = (0..64).map(|_| c.next_u64()).collect();
    assert_eq!(first, second);
    assert_ne!(first, third);
    let mut rng = Rng::new(7);
    for _ in 0..10_000 {
        let unit = rng.unit();
        assert!((0.0..1.0).contains(&unit));
        assert!(rng.normal().is_finite());
    }
}

#[test]
fn log_normal_respects_bounds_and_centres_on_median() {
    for profile in [&NATURAL, &CAUTIOUS] {
        let mut rng = Rng::new(11);
        let mut samples: Vec<f64> = (0..20_000)
            .map(|_| rng.log_normal(profile.gap_median_ms, profile.gap_sigma, profile.gap_ms))
            .collect();
        assert!(
            samples
                .iter()
                .all(|value| (profile.gap_ms.0..=profile.gap_ms.1).contains(value))
        );
        samples.sort_by(f64::total_cmp);
        let median = samples[samples.len() / 2];
        assert!(
            (median / profile.gap_median_ms - 1.0).abs() < 0.05,
            "{median}"
        );
        // Right-skewed like human reaction times.
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        assert!(mean > median);
    }
}

#[test]
fn off_is_a_strict_no_op() {
    let mut pacer = Pacer::new(Policy::Off, 1);
    let before = pacer.rng.clone().next_u64();
    let mut paced = pacer.begin(None);
    assert_eq!(paced.policy, Policy::Off);
    let action = tap(10.5, 20.25);
    let frames = [[0.0, 0.0, 100.0, 100.0]];
    let shaped = paced.shape_action(action.clone(), &frames);
    assert_eq!(
        serde_json::to_value(&shaped).unwrap(),
        serde_json::to_value(&action).unwrap()
    );
    assert!(paced.shape_for(&action).is_none());
    // No randomness was consumed.
    assert_eq!(pacer.rng.clone().next_u64(), before);
    let mut rng = Rng::new(1);
    assert_eq!(
        pacer.wait_before(Policy::Off, &mut rng, Instant::now()),
        Duration::ZERO
    );
    // An explicit off override disables a paced session for one call.
    let mut paced = Pacer::new(Policy::Natural, 1)
        .without_sleep()
        .begin(Some(Override {
            policy: Some(Policy::Off),
            seed: None,
        }));
    assert!(paced.shape_for(&action).is_none());
    // Off has no daemon motion and cannot be sent on the wire.
    assert!(Policy::Off.motion().is_none());
    assert!(serde_json::from_value::<Shape>(json!({"policy":"off","seed":1})).is_err());
    assert!(serde_json::from_value::<Shape>(json!({"policy":"natural","seed":1,"x":1})).is_err());
    assert_eq!(
        serde_json::from_value::<Shape>(json!({"policy":"cautious","seed":7})).unwrap(),
        Shape {
            policy: MotionPolicy::Cautious,
            seed: 7
        }
    );
}

#[test]
fn taps_always_land_strictly_inside_the_frame() {
    let frames = [
        [100.0, 200.0, 44.0, 44.0],
        [0.0, 0.0, 390.0, 844.0],
        [10.0, 10.0, 3.0, 3.0],
        [300.0, 500.0, 80.0, 12.0],
    ];
    for profile in [&NATURAL, &CAUTIOUS] {
        let mut rng = Rng::new(3);
        for index in 0..20_000 {
            let frame = frames[index % frames.len()];
            let at = Point {
                x: frame[0] + frame[2] * (0.01 + 0.98 * rng.unit()),
                y: frame[1] + frame[3] * (0.01 + 0.98 * rng.unit()),
            };
            let point = jitter_tap(&mut rng, profile, at, &frames);
            // The smallest containing frame bounds the result.
            let smallest = frames
                .iter()
                .filter(|f| strictly_inside(at, **f))
                .min_by(|a, b| (a[2] * a[3]).total_cmp(&(b[2] * b[3])))
                .unwrap();
            assert!(strictly_inside(point, *smallest), "{at:?} -> {point:?}");
            assert!((point.x - at.x).abs() <= profile.tap_max_shift_pt + 1e-9);
            assert!((point.y - at.y).abs() <= profile.tap_max_shift_pt + 1e-9);
        }
    }
}

#[test]
fn taps_outside_any_frame_or_in_tiny_frames_are_unchanged() {
    let mut rng = Rng::new(5);
    let at = Point { x: 50.0, y: 50.0 };
    for frames in [
        vec![],
        vec![[0.0, 0.0, 10.0, 10.0]],
        vec![[50.0, 0.0, 10.0, 100.0]],
        vec![[49.5, 49.5, 1.0, 1.0]],
        vec![[f64::NAN, 0.0, 100.0, 100.0]],
        vec![[0.0, 0.0, -100.0, 100.0]],
    ] {
        for _ in 0..100 {
            assert_eq!(jitter_tap(&mut rng, &NATURAL, at, &frames), at);
        }
    }
}

#[test]
fn tap_press_duration_varies_within_bounds() {
    for policy in POLICIES {
        let profile = policy.profile().unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for seed in 0..2_000 {
            let (path, body) = shaped_payload(&tap(10.0, 10.0), shape(policy, seed)).unwrap();
            assert_eq!(path, "/actions");
            let steps = &body["actions"][0]["actions"];
            assert_eq!(steps[0]["x"], 10.0);
            let press = steps[2]["duration"].as_u64().unwrap();
            assert!((ms(profile.press_ms.0)..=ms(profile.press_ms.1)).contains(&press));
            seen.insert(press);
        }
        assert!(seen.len() > 20, "{policy:?} press durations too regular");
    }
}

#[test]
fn double_tap_keeps_one_point_and_bounded_timing() {
    for policy in POLICIES {
        let profile = policy.profile().unwrap();
        let (_, body) = shaped_payload(
            &Action::DoubleTap {
                at: Point { x: 7.0, y: 8.0 },
            },
            shape(policy, 4),
        )
        .unwrap();
        let steps = body["actions"][0]["actions"].as_array().unwrap();
        assert_eq!(steps.len(), 8);
        let gap = steps[4]["duration"].as_u64().unwrap();
        assert!((ms(profile.double_tap_gap_ms.0)..=ms(profile.double_tap_gap_ms.1)).contains(&gap));
    }
}

#[test]
fn swipes_keep_endpoints_and_duration_and_curve_within_bounds() {
    let from = Point { x: 195.0, y: 700.0 };
    let to = Point { x: 195.0, y: 200.0 };
    for policy in POLICIES {
        let profile = policy.profile().unwrap();
        for seed in 0..500 {
            let mut rng = Rng::new(seed);
            let path = swipe_path(&mut rng, profile, from, to, 450).unwrap();
            assert_eq!(path.last().unwrap().0, to);
            assert_eq!(path.iter().map(|(_, d)| d).sum::<u64>(), 450);
            let distance = 500.0;
            let max_bulge = (profile.curve_fraction.1 * distance).min(profile.curve_max_pt);
            for (point, _) in &path {
                // Perpendicular offset from the straight line x = 195.
                assert!((point.x - 195.0).abs() <= max_bulge + 1e-6, "{point:?}");
                assert!((200.0..=700.0).contains(&point.y));
            }
            // Progress is monotonic toward the end point.
            assert!(path.windows(2).all(|pair| pair[1].0.y <= pair[0].0.y));
            let (_, body) = shaped_payload(
                &Action::Swipe {
                    from,
                    to,
                    duration_ms: 450,
                },
                shape(policy, seed),
            )
            .unwrap();
            let steps = body["actions"][0]["actions"].as_array().unwrap();
            assert_eq!(steps[0]["x"], from.x);
            assert_eq!(steps[0]["y"], from.y);
            let last_move = &steps[steps.len() - 2];
            assert_eq!(last_move["x"], to.x);
            assert_eq!(last_move["y"], to.y);
            assert_eq!(steps.last().unwrap()["type"], "pointerUp");
        }
    }
    // Paths are actually curved and velocity actually varies.
    let mut rng = Rng::new(1);
    let path = swipe_path(&mut rng, &NATURAL, from, to, 450).unwrap();
    assert!(path.iter().any(|(point, _)| (point.x - 195.0).abs() > 1.0));
    let durations: std::collections::BTreeSet<_> = path.iter().map(|(_, d)| *d).collect();
    assert!(durations.len() > 2);
    // Zero-length swipes keep the existing hold path.
    assert!(
        shaped_payload(
            &Action::Swipe {
                from,
                to: from,
                duration_ms: 450
            },
            Shape {
                policy: MotionPolicy::Natural,
                seed: 1
            }
        )
        .is_none()
    );
}

#[test]
fn typing_cadence_is_bursty_bounded_and_ascii_only() {
    for policy in POLICIES {
        let profile = policy.profile().unwrap();
        let mut rng = Rng::new(8);
        let text = "hello world, this is a test";
        let plan = key_plan(&mut rng, profile, text).unwrap();
        assert_eq!(plan.iter().map(|key| key.0).collect::<String>(), text);
        assert_eq!(plan.last().unwrap().2, 0);
        for (_, hold, gap) in &plan {
            assert!((ms(profile.key_hold_ms.0)..=ms(profile.key_hold_ms.1)).contains(hold));
            assert!(*gap == 0 || *gap >= ms(profile.key_ms.0));
            assert!(*gap <= ms(profile.burst_pause_ms.1));
        }
        // Bursts: some pauses are much longer than in-burst gaps.
        assert!(plan.iter().any(|key| key.2 >= ms(profile.burst_pause_ms.0)));
        assert!(
            plan.iter()
                .any(|key| key.2 > 0 && key.2 < ms(profile.burst_pause_ms.0))
        );
        for skipped in ["café", "👩‍💻", &"a".repeat(MAX_SHAPED_TEXT_CHARS + 1)] {
            assert!(key_plan(&mut rng, profile, skipped).is_none());
        }
    }
    // Non-ASCII text stays one whole string at a bounded frequency.
    let (path, body) = shaped_payload(
        &Action::Text {
            value: "👩‍💻 café".into(),
        },
        Shape {
            policy: MotionPolicy::Natural,
            seed: 2,
        },
    )
    .unwrap();
    assert_eq!(path, "/wda/keys");
    assert_eq!(body["value"], json!(["👩‍💻 café"]));
    let frequency = body["frequency"].as_u64().unwrap();
    assert!((120..=330).contains(&frequency));
    let (path, body) = shaped_payload(
        &Action::TextCurrent { value: "ok".into() },
        Shape {
            policy: MotionPolicy::Cautious,
            seed: 2,
        },
    )
    .unwrap();
    assert_eq!(path, "/actions");
    assert_eq!(body["actions"][0]["type"], "key");
}

#[test]
fn shaped_payloads_are_seed_deterministic() {
    let actions = [
        tap(10.0, 20.0),
        Action::DoubleTap {
            at: Point { x: 1.0, y: 2.0 },
        },
        Action::Swipe {
            from: Point { x: 10.0, y: 500.0 },
            to: Point { x: 300.0, y: 120.0 },
            duration_ms: 600,
        },
        Action::Text {
            value: "deterministic".into(),
        },
    ];
    for policy in POLICIES {
        for action in &actions {
            let a = shaped_payload(action, shape(policy, 77));
            let b = shaped_payload(action, shape(policy, 77));
            let c = shaped_payload(action, shape(policy, 78));
            assert_eq!(a, b);
            assert_ne!(a, c, "{}", action.kind());
        }
    }
    // The MCP side replays identically from the same seed too.
    let call = Some(Override {
        policy: Some(Policy::Natural),
        seed: Some(5),
    });
    let frames = [[0.0, 0.0, 200.0, 200.0]];
    let run = || {
        let mut pacer = Pacer::new(Policy::Off, 0).without_sleep();
        let mut paced = pacer.begin(call);
        let action = paced.shape_action(tap(100.0, 100.0), &frames);
        (
            serde_json::to_value(&action).unwrap(),
            paced.shape_for(&action),
        )
    };
    assert_eq!(run(), run());
}

#[test]
fn unsupported_or_invalid_actions_are_never_shaped() {
    let shape = Shape {
        policy: MotionPolicy::Natural,
        seed: 1,
    };
    for action in [
        Action::Home,
        Action::Lock,
        Action::Key {
            key: handkite_core::protocol::KeyboardKey::Enter,
        },
        tap(-1.0, 2.0),
        tap(f64::NAN, 2.0),
        Action::Text {
            value: "a\nb".into(),
        },
        Action::Swipe {
            from: Point { x: 1.0, y: 1.0 },
            to: Point { x: 100.0, y: 1.0 },
            duration_ms: 0,
        },
    ] {
        assert!(
            shaped_payload(&action, shape).is_none(),
            "{}",
            action.kind()
        );
    }
    let mut paced = Pacer::new(Policy::Natural, 1).without_sleep().begin(None);
    for action in [
        Action::Home,
        Action::Key {
            key: handkite_core::protocol::KeyboardKey::Enter,
        },
    ] {
        assert!(paced.shape_for(&action).is_none());
    }
}

#[test]
fn long_press_and_swipe_durations_stay_bounded() {
    for policy in POLICIES {
        let profile = policy.profile().unwrap();
        let mut pacer = Pacer::new(policy, 9).without_sleep();
        for _ in 0..2_000 {
            let mut paced = pacer.begin(None);
            let Action::LongPress { duration_ms, .. } = paced.shape_action(
                Action::LongPress {
                    at: Point { x: 5.0, y: 5.0 },
                    duration_ms: 800,
                },
                &[],
            ) else {
                panic!()
            };
            let spread = 800.0 * profile.long_press_spread;
            assert!((f64::from(duration_ms) - 800.0).abs() <= spread + 1.0);
            for requested in [1, 300, 5_000] {
                let Action::Swipe { duration_ms, .. } = paced.shape_action(
                    Action::Swipe {
                        from: Point { x: 1.0, y: 1.0 },
                        to: Point { x: 2.0, y: 2.0 },
                        duration_ms: requested,
                    },
                    &[],
                ) else {
                    panic!()
                };
                assert!((1..=5_000).contains(&duration_ms));
                let ratio = f64::from(duration_ms) / f64::from(requested);
                if requested == 300 {
                    assert!(ratio >= profile.swipe_factor.0 - 0.01);
                    assert!(ratio <= profile.swipe_factor.1 + 0.01);
                }
            }
        }
    }
}

#[test]
fn gap_counts_time_already_spent_and_cap_limits_actions_per_minute() {
    let start = Instant::now();
    let mut rng = Rng::new(1);
    let mut pacer = Pacer::new(Policy::Natural, 0);
    // The first action never waits for a gap.
    assert_eq!(
        pacer.wait_before(Policy::Natural, &mut rng, start),
        Duration::ZERO
    );
    pacer.record(start);
    let wait = pacer.wait_before(Policy::Natural, &mut rng, start);
    assert!(wait >= Duration::from_millis(ms(NATURAL.gap_ms.0)));
    assert!(wait <= Duration::from_millis(ms(NATURAL.gap_ms.1)));
    // A caller that already thought longer than the maximum gap is not delayed.
    let later = start + Duration::from_secs(3);
    assert_eq!(
        pacer.wait_before(Policy::Natural, &mut rng, later),
        Duration::ZERO
    );

    for (policy, cap) in [
        (Policy::Natural, NATURAL.actions_per_minute),
        (Policy::Cautious, CAUTIOUS.actions_per_minute),
    ] {
        let mut pacer = Pacer::new(policy, 0);
        // `cap` actions spaced past the maximum gap fill the window.
        let spacing = Duration::from_millis(60_000 / cap as u64 / 2);
        let mut now = start;
        for _ in 0..cap {
            pacer.record(now);
            now += spacing;
        }
        let wait = pacer.wait_before(policy, &mut rng, now);
        let expected = (start + WINDOW).saturating_duration_since(now);
        assert!(wait >= expected, "{policy:?}");
        // Once the window passes, the cap releases.
        let after = start + WINDOW + Duration::from_secs(10);
        assert!(pacer.wait_before(policy, &mut rng, after) < Duration::from_secs(5));
    }
}

#[test]
fn begin_sleeps_only_when_paced() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SLEPT: AtomicU64 = AtomicU64::new(0);
    let mut pacer = Pacer::new(Policy::Off, 0);
    pacer.sleep = |duration| {
        SLEPT.fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
    };
    for _ in 0..5 {
        pacer.begin(None);
    }
    assert_eq!(SLEPT.load(Ordering::SeqCst), 0);
    pacer.begin(Some(Override {
        policy: Some(Policy::Cautious),
        seed: Some(1),
    }));
    assert!(SLEPT.load(Ordering::SeqCst) >= ms(CAUTIOUS.gap_ms.0));
}
