use super::*;

fn input() -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1, "pid": 42, "start_seconds": 123,
        "start_microseconds": 456, "start_abstime": 789, "effective_uid": 501,
        "executable_path": "/test/controller",
        "executable_uuid": "00112233445566778899aabbccddeeff",
    })
}

fn expected() -> Expected {
    Expected::parse(&serde_json::to_vec(&input()).unwrap()).unwrap()
}

#[test]
fn strict_object_rejects_duplicate_unknown_missing_and_mistyped_fields() {
    for bytes in [b"[]".as_slice(), b"null", b"{}", b"\xff"] {
        assert!(Expected::parse(bytes).is_none());
    }
    let base = serde_json::to_string(&input()).unwrap();
    assert!(Expected::parse(format!("{base}{{}}").as_bytes()).is_none());
    let duplicate = base.replacen('{', "{\"pid\":42,", 1);
    assert!(Expected::parse(duplicate.as_bytes()).is_none());
    for key in input().as_object().unwrap().keys() {
        let mut value = input();
        value.as_object_mut().unwrap().remove(key);
        assert!(Expected::parse(&serde_json::to_vec(&value).unwrap()).is_none());
        value[key] = serde_json::Value::Null;
        assert!(Expected::parse(&serde_json::to_vec(&value).unwrap()).is_none());
    }
    let mut value = input();
    value["fallback"] = true.into();
    assert!(Expected::parse(&serde_json::to_vec(&value).unwrap()).is_none());
}

#[test]
fn rejects_invalid_bounds_and_noncanonical_path_or_uuid() {
    for (key, bad) in [
        ("schema_version", 2.into()),
        ("pid", 0.into()),
        ("pid", (-1).into()),
        ("pid", 2_147_483_648_u64.into()),
        ("start_seconds", 0.into()),
        ("start_microseconds", 1_000_000.into()),
        ("start_abstime", 0.into()),
        ("effective_uid", (-1).into()),
        ("executable_uuid", "0".repeat(32).into()),
        ("executable_uuid", "A".repeat(32).into()),
        ("executable_uuid", "g".repeat(32).into()),
        ("executable_uuid", "a".repeat(31).into()),
    ] {
        let mut value = input();
        value[key] = bad;
        assert!(Expected::parse(&serde_json::to_vec(&value).unwrap()).is_none());
    }
    for path in [
        "", "/", "relative", "//test", "/a//b", "/a/./b", "/a/../b", "/a/", "/a\0b", "/a\nb",
    ] {
        let mut value = input();
        value["executable_path"] = path.into();
        assert!(Expected::parse(&serde_json::to_vec(&value).unwrap()).is_none());
    }
    let mut value = input();
    value["executable_path"] = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1)).into();
    assert!(Expected::parse(&serde_json::to_vec(&value).unwrap()).is_none());
    let mut bytes = serde_json::to_vec(&input()).unwrap();
    bytes.resize(MAX_INPUT_BYTES, b' ');
    assert!(Expected::parse(&bytes).is_some());
    bytes.push(b' ');
    assert!(Expected::parse(&bytes).is_none());
}

#[test]
fn exactly_two_complete_equal_samples_match_only_all_pins() {
    let expected = expected();
    let mut calls = 0;
    assert_eq!(
        compare(&expected, |pid| {
            calls += 1;
            assert_eq!(pid, 42);
            Ok(self::expected().identity)
        }),
        Observation::Match
    );
    assert_eq!(calls, 2);
    for field in [
        "start_seconds",
        "start_microseconds",
        "start_abstime",
        "effective_uid",
        "executable_path",
        "executable_uuid",
    ] {
        let mut value = input();
        value[field] = match field {
            "executable_path" => "/test/other".into(),
            "executable_uuid" => "a".repeat(32).into(),
            _ => 999.into(),
        };
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            compare(&expected, |_| Ok(Expected::parse(&bytes).unwrap().identity)),
            Observation::Mismatch
        );
        let mut calls = 0;
        assert_eq!(
            compare(&expected, |_| {
                calls += 1;
                if calls == 1 {
                    Ok(self::expected().identity)
                } else {
                    Ok(Expected::parse(&bytes).unwrap().identity)
                }
            }),
            Observation::Unknown
        );
    }
}

#[test]
fn disappearance_permission_failure_and_mixed_absence_are_unknown() {
    let expected = expected();
    for outcomes in [
        vec![Ok(self::expected().identity), Err(ReadFailure::Absent)],
        vec![Ok(self::expected().identity), Err(ReadFailure::Unknown)],
        vec![Err(ReadFailure::Absent), Ok(self::expected().identity)],
        vec![Err(ReadFailure::Absent), Err(ReadFailure::Unknown)],
    ] {
        let mut outcomes = outcomes.into_iter();
        assert_eq!(
            compare(&expected, |_| outcomes.next().unwrap()),
            Observation::Unknown
        );
        assert!(outcomes.next().is_none());
    }
    let mut calls = 0;
    assert_eq!(
        compare(&expected, |_| {
            calls += 1;
            Err(ReadFailure::Unknown)
        }),
        Observation::Unknown
    );
    assert_eq!(calls, 1);
    let mut calls = 0;
    assert_eq!(
        compare(&expected, |_| {
            calls += 1;
            Err(ReadFailure::Absent)
        }),
        Observation::Absent
    );
    assert_eq!(calls, 2);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn unsupported_platform_is_unknown() {
    assert_eq!(observe(&expected()), Observation::Unknown);
}
