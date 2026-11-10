use super::*;
use device_health::DeviceHealth;
use handkite_core::protocol::LockState;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::NamedTempFile;

fn config(contents: &[u8]) -> NamedTempFile {
    let mut file = NamedTempFile::new().unwrap();
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    file.write_all(contents).unwrap();
    file
}

fn valid_config() -> NamedTempFile {
    config(br#"{"device_key":"phone","usb_id":"fixture-usb"}"#)
}

fn check_args(file: &Path) -> Vec<OsString> {
    vec!["--check".into(), "--config".into(), file.into()]
}

#[test]
fn service_layout_check_does_not_inspect_or_serve_a_device() {
    let mut output = Vec::new();
    assert_eq!(
        run_with_layout_check(
            &["--check-service-layout".into()],
            |_| panic!("layout check must not inspect a device"),
            |_, _| panic!("layout check must not serve"),
            |layout| {
                assert_eq!(layout, &ServiceLayout::current_account().unwrap());
                Ok(())
            },
            &mut output,
        ),
        Ok(0)
    );
    assert_eq!(output, b"service_layout=ready\n");
    output.clear();
    assert_eq!(
        run_with_layout_check(
            &["--check-service-layout".into()],
            |_| panic!("failed layout check must not inspect a device"),
            |_, _| panic!("failed layout check must not serve"),
            |_| Err("protected service configuration unavailable".into()),
            &mut output,
        ),
        Err("protected service configuration unavailable".into())
    );
    assert!(output.is_empty());
}

fn ready() -> DeviceHealth {
    DeviceHealth {
        usb_connected: true,
        wda_http: true,
        wda_ready: true,
        existing_session_valid: true,
        native_lock: LockState::Locked,
        unlocked_since_boot: Some(true),
    }
}

#[test]
fn check_reports_only_health_and_never_serves() {
    let config = valid_config();
    for lock in [LockState::Locked, LockState::Unlocked] {
        for unlocked_since_boot in [None, Some(false), Some(true)] {
            let health = DeviceHealth {
                native_lock: lock,
                unlocked_since_boot,
                ..ready()
            };
            let mut output = Vec::new();
            let code = run(
                &check_args(config.path()),
                |id| {
                    assert_eq!(id.as_str(), "fixture-usb");
                    health
                },
                |_, _| panic!("check must not initialize or serve the daemon"),
                &mut output,
            )
            .unwrap();
            assert_eq!(code, 0);
            let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(json, serde_json::to_value(health).unwrap());
            assert_eq!(json.as_object().unwrap().len(), 6);
            assert!(output.len() < 200);
            assert!(!String::from_utf8(output).unwrap().contains("fixture-usb"));
        }
    }
}

#[test]
fn every_readiness_failure_returns_one_without_serving() {
    let config = valid_config();
    for health in [
        DeviceHealth {
            usb_connected: false,
            ..ready()
        },
        DeviceHealth {
            wda_http: false,
            ..ready()
        },
        DeviceHealth {
            wda_ready: false,
            ..ready()
        },
        DeviceHealth {
            existing_session_valid: false,
            ..ready()
        },
        DeviceHealth {
            native_lock: LockState::Uncertain,
            ..ready()
        },
    ] {
        let mut output = Vec::new();
        assert_eq!(
            run(
                &check_args(config.path()),
                |_| health,
                |_, _| panic!("failed check must not serve"),
                &mut output,
            )
            .unwrap(),
            1,
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
            serde_json::to_value(health).unwrap(),
        );
    }
}

fn rejected_before_device_access(args: &[OsString]) -> String {
    let mut output = Vec::new();
    let error = run(
        args,
        |_| panic!("invalid input must not inspect a device"),
        |_, _| panic!("invalid input must not serve"),
        &mut output,
    )
    .unwrap_err();
    assert!(output.is_empty());
    error
}

#[test]
fn invalid_config_is_rejected_before_device_access() {
    for contents in [
        b"invalid".as_slice(),
        br#"{"device_key":"../phone","usb_id":"fixture-usb"}"#,
        br#"{"device_key":"phone","usb_id":""}"#,
        br#"{"device_key":"phone","usb_id":"fixture-usb","extra":true}"#,
    ] {
        let config = config(contents);
        let error = rejected_before_device_access(&check_args(config.path()));
        assert!(!error.contains("fixture-usb"));
    }
    let config = valid_config();
    fs::set_permissions(config.path(), fs::Permissions::from_mode(0o644)).unwrap();
    rejected_before_device_access(&check_args(config.path()));
    let path = config.path().to_owned();
    config.close().unwrap();
    rejected_before_device_access(&check_args(&path));
}

#[test]
fn only_exact_command_forms_are_accepted() {
    let config = valid_config();
    let path = config.path().as_os_str().to_owned();
    for args in [
        vec!["--check".into()],
        vec!["--check".into(), "--config".into()],
        vec!["--config".into(), path.clone(), "--check".into()],
        vec![
            "--check".into(),
            "--config".into(),
            path.clone(),
            "--check".into(),
        ],
        vec![
            "--check".into(),
            "--config".into(),
            path.clone(),
            "--socket-dir".into(),
            "/unused".into(),
        ],
        vec![
            "--config".into(),
            path,
            "--socket-dir".into(),
            "/unused".into(),
            "--unknown".into(),
        ],
    ] {
        assert_eq!(rejected_before_device_access(&args), USAGE);
    }
}

#[test]
fn normal_daemon_mode_uses_only_fixed_service_paths() {
    let layout = ServiceLayout::current_account().unwrap();
    for args in [
        vec![],
        vec![
            "--config".into(),
            layout.config_file().into(),
            "--socket-dir".into(),
            layout.runtime_directory().into(),
        ],
    ] {
        let mut output = Vec::new();
        let code = run(
            &args,
            |_| panic!("normal daemon startup does not run a separate preflight"),
            |resolved, mode| {
                assert_eq!(resolved, layout);
                assert_eq!(mode, DaemonMode::Borrowed);
                Ok(())
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(output.is_empty());
    }
}

#[test]
fn owned_daemon_requires_one_explicit_flag_and_fixed_paths() {
    let layout = ServiceLayout::current_account().unwrap();
    let mut output = Vec::new();
    assert_eq!(
        run(
            &["--owned-wda".into()],
            |_| panic!("owned startup must not run a separate preflight"),
            |resolved, mode| {
                assert_eq!(resolved, layout);
                assert_eq!(mode, DaemonMode::Owned);
                Ok(())
            },
            &mut output,
        ),
        Ok(0)
    );
    assert!(output.is_empty());
    for args in [
        vec!["--owned-wda".into(), "--owned-wda".into()],
        vec!["--owned-wda".into(), "--config".into()],
    ] {
        assert_eq!(rejected_before_device_access(&args), USAGE);
    }
}

#[test]
fn alternate_config_or_socket_directory_is_rejected_before_device_access() {
    let config = valid_config();
    let scratch = tempfile::tempdir().unwrap();
    let layout = ServiceLayout::current_account().unwrap();
    for (config, runtime) in [
        (config.path().to_owned(), layout.runtime_directory()),
        (layout.config_file(), scratch.path().to_owned()),
        (config.path().to_owned(), scratch.path().to_owned()),
    ] {
        let error = rejected_before_device_access(&[
            "--config".into(),
            config.into(),
            "--socket-dir".into(),
            runtime.into(),
        ]);
        assert_eq!(
            error,
            "daemon requires the fixed service configuration and runtime paths"
        );
    }
    assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
}

#[test]
fn protected_config_failure_stops_before_socket_or_device_startup() {
    let layout = ServiceLayout::current_account().unwrap();
    let error = load_before_serving(
        &layout,
        |resolved| {
            assert_eq!(resolved, &layout);
            Err("unsafe or changed service configuration")
        },
        |_| panic!("config failure must not bind a socket or initialize a device"),
    )
    .unwrap_err();
    assert_eq!(error, "unsafe or changed service configuration");
}

#[test]
fn startup_continues_once_with_the_loaded_binding() {
    let layout = ServiceLayout::current_account().unwrap();
    let calls = std::cell::RefCell::new(Vec::new());
    let result = load_before_serving(
        &layout,
        |resolved| {
            calls.borrow_mut().push("load");
            assert_eq!(resolved, &layout);
            Ok(DeviceBinding {
                key: handkite_core::protocol::DeviceKey::new("phone").unwrap(),
                usb_id: HardwareId::new("fixture-usb").unwrap(),
            })
        },
        |binding| {
            calls.borrow_mut().push("serve");
            assert_eq!(binding.key.as_str(), "phone");
            assert_eq!(binding.usb_id.as_str(), "fixture-usb");
            Err("fixture continuation".into())
        },
    );
    assert_eq!(result, Err("fixture continuation".into()));
    assert_eq!(*calls.borrow(), ["load", "serve"]);
}

#[test]
fn socket_refusal_prevents_device_startup() {
    let mut calls = Vec::new();
    let result = bind_before_start(
        || {
            calls.push("bind");
            Err::<(), _>("unsafe socket".into())
        },
        |_| panic!("device startup must not run after socket refusal"),
    );
    assert_eq!(result, Err("unsafe socket".into()));
    assert_eq!(calls, ["bind"]);
}
