use handkite_core::controller::Controller;
use handkite_core::protocol::{
    Action, ActionRequest, DeviceBinding, DeviceKey, HardwareId, LockState, Observation,
};
use handkite_core::unlock::PasscodeBackend;
use handkite_host::pin_backend::NativePasscodeBackend;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_adapter::WdaAdapter;
use handkite_host::wda_transport::UsbWda;
use handkite_host::{native_lock, pin_screen};
use std::time::Duration;

fn main() {
    let expected = match std::env::var("HANDKITE_DEVICE_UDID")
        .ok()
        .and_then(|value| HardwareId::new(value).ok())
    {
        Some(expected) => expected,
        None => {
            eprintln!("HANDKITE_DEVICE_UDID is required");
            std::process::exit(2);
        }
    };
    let command = std::env::args().nth(1).unwrap_or_else(|| "status".into());
    let result = match command.as_str() {
        "status" | "sessions" | "pin-attrs" | "active-session" => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Tokio runtime");
            let wda = UsbWda::new(PinnedUsb::new(expected));
            match command.as_str() {
                "status" => runtime.block_on(status(&wda)),
                "sessions" => runtime.block_on(sessions(&wda)),
                "active-session" => runtime.block_on(active_session(&wda)),
                _ => runtime.block_on(pin_attrs(&wda)),
            }
        }
        "observe" => observe(expected),
        "demo" => demo(expected),
        "home" => home(expected),
        "native-lock" => {
            let result = native_lock::query(&expected);
            println!(
                "native_lock={:?} unlocked_since_boot={:?}",
                result.state, result.unlocked_since_boot
            );
            Ok(())
        }
        "pin-shape" => pin_shape(expected),
        "show-keypad" => show_keypad(expected),
        _ => Err("unknown probe command".into()),
    };
    if let Err(error) = result {
        eprintln!("probe_error={error}");
        std::process::exit(1);
    }
}

async fn status(wda: &UsbWda) -> Result<(), String> {
    let response = wda
        .request("GET", "/status", b"", Duration::from_secs(8))
        .await
        .map_err(|error| format!("{error:?}"))?;
    let ready = serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()
        .is_some_and(|value| value["value"]["ready"] == true);
    println!("http_status={} wda_ready={ready}", response.status);
    (response.status == 200 && ready)
        .then_some(())
        .ok_or_else(|| "WDA is not ready".into())
}

async fn sessions(wda: &UsbWda) -> Result<(), String> {
    let response = wda
        .request("GET", "/sessions", b"", Duration::from_secs(8))
        .await
        .map_err(|error| format!("{error:?}"))?;
    let value: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| "invalid WDA JSON")?;
    let sessions = value["value"].as_array();
    let valid_ids = sessions.is_some_and(|sessions| {
        sessions.iter().all(|session| {
            session["id"]
                .as_str()
                .or_else(|| session["sessionId"].as_str())
                .is_some()
        })
    });
    println!(
        "sessions_status={} count={} valid_ids={valid_ids}",
        response.status,
        sessions.map_or(0, Vec::len)
    );
    Ok(())
}

async fn pin_attrs(wda: &UsbWda) -> Result<(), String> {
    let started = std::time::Instant::now();
    let response = wda
        .request("GET", "/source?format=xml&excluded_attributes=accessible,focused,selected,index,traits,placeholderValue,hittable,customActions,nativeFrame", b"", Duration::from_secs(10))
        .await
        .map_err(|error| format!("{error:?}"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| "invalid WDA JSON")?;
    let xml = json["value"].as_str().ok_or("missing source")?;
    println!(
        "pin_source_status={} visible_attr={} enabled_attr={} secure_field={} digit_key={} elapsed_ms={}",
        response.status,
        xml.contains(" visible=\""),
        xml.contains(" enabled=\""),
        xml.contains("XCUIElementTypeSecureTextField"),
        xml.contains("XCUIElementTypeKey"),
        started.elapsed().as_millis()
    );
    Ok(())
}

async fn active_session(wda: &UsbWda) -> Result<(), String> {
    let response = wda
        .request("GET", "/status", b"", Duration::from_secs(8))
        .await
        .map_err(|error| format!("{error:?}"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|_| "invalid WDA JSON")?;
    let id = json["sessionId"].as_str().filter(|id| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    let Some(id) = id else {
        println!("active_session=false validated=false");
        return Ok(());
    };
    let path = format!("/session/{id}");
    let validation = wda
        .request("GET", &path, b"", Duration::from_secs(8))
        .await
        .map_err(|error| format!("{error:?}"))?;
    println!("active_session=true validated={}", validation.status == 200);
    Ok(())
}

fn observe(expected: HardwareId) -> Result<(), String> {
    let (controller, key) = controller(expected)?;
    let observation = controller
        .observe(&key)
        .map_err(|error| format!("{error:?}"))?;
    let png = &observation.screenshot.bytes;
    if png.len() < 24 {
        return Err("screenshot is truncated".into());
    }
    let width = u32::from_be_bytes(png[16..20].try_into().map_err(|_| "invalid PNG width")?);
    let height = u32::from_be_bytes(png[20..24].try_into().map_err(|_| "invalid PNG height")?);
    println!(
        "revision={} screenshot_png={}x{} source_nonempty={} lock={:?} window_ms={}",
        observation.revision.0,
        width,
        height,
        !observation.accessibility_tree.is_empty(),
        observation.state.lock,
        observation
            .window
            .duration_ms()
            .map_err(|_| "invalid window")?
    );
    Ok(())
}

fn controller(expected: HardwareId) -> Result<(Controller<WdaAdapter>, DeviceKey), String> {
    let key = DeviceKey::new("iphone-15-pro").map_err(|error| error.to_string())?;
    let adapter = WdaAdapter::new(expected.clone()).map_err(|_| "cannot initialize WDA adapter")?;
    let controller = Controller::new(
        DeviceBinding {
            key: key.clone(),
            usb_id: expected,
        },
        adapter,
        5000,
        5000,
    );
    Ok((controller, key))
}

fn pin_shape(expected: HardwareId) -> Result<(), String> {
    let (controller, key) = controller(expected)?;
    let observation = controller
        .observe(&key)
        .map_err(|error| format!("{error:?}"))?;
    let parsed = pin_screen::parse(&observation.accessibility_tree)
        .map_err(|_| "cannot parse passcode screen")?;
    println!(
        "visible_lock={:?} secure_field={} keypad={} delete_key={}",
        parsed.visible_lock,
        parsed.secure_field_value.is_some(),
        parsed.keypad.is_some(),
        parsed.delete_key.is_some()
    );
    Ok(())
}

fn show_keypad(expected: HardwareId) -> Result<(), String> {
    let mut backend =
        NativePasscodeBackend::new(expected).map_err(|_| "cannot initialize passcode backend")?;
    let before = backend
        .snapshot()
        .map_err(|error| format!("snapshot_before={error:?}"))?;
    if before.native_lock != LockState::Locked
        || before.visible_lock != LockState::Locked
        || before.before_first_unlock
        || before.trust_prompt
    {
        return Err("passcode prompt precondition unavailable".into());
    }
    backend
        .show_keypad()
        .map_err(|error| format!("show_keypad={error:?}"))?;
    let after = backend
        .snapshot()
        .map_err(|error| format!("snapshot_after={error:?}"))?;
    println!(
        "native_lock={:?} visible_lock={:?} keypad_ready={} secure_field={} trust_prompt={}",
        after.native_lock,
        after.visible_lock,
        after.keypad_ready,
        after.secure_field_value.is_some(),
        after.trust_prompt,
    );
    if after.native_lock != LockState::Locked
        || after.visible_lock != LockState::Locked
        || !after.keypad_ready
        || after.trust_prompt
    {
        return Err("passcode keypad unconfirmed".into());
    }
    Ok(())
}

fn demo(expected: HardwareId) -> Result<(), String> {
    let (controller, key) = controller(expected)?;
    let before = controller
        .observe(&key)
        .map_err(|error| format!("{error:?}"))?;
    if before.state.lock != LockState::Unlocked
        || before.state.foreground_app.as_deref() != Some("com.apple.mobiletimer")
    {
        return Err("demo requires unlocked Clock in foreground".into());
    }
    let owner = "local-demo";
    let lease = controller
        .acquire(owner.into(), Duration::from_secs(60))
        .map_err(|error| format!("{error:?}"))?;
    let settings = controller
        .act(
            owner,
            lease,
            ActionRequest {
                device: key.clone(),
                expected_revision: Some(before.revision),
                action: Action::LaunchApp {
                    bundle_id: "com.apple.Preferences".into(),
                },
            },
        )
        .map_err(|error| format!("{error:?}"))?;
    if !settings.receipt.accepted {
        return Err("Settings request unaccepted".into());
    }
    let after_settings = settle_foreground(&controller, &key, "com.apple.Preferences")?;
    if after_settings.state.foreground_app.as_deref() != Some("com.apple.Preferences") {
        return Err("Settings effect unconfirmed".into());
    }
    println!("settings_receipt=true settings_observed=true");
    let home = controller
        .act(
            owner,
            lease,
            ActionRequest {
                device: key.clone(),
                expected_revision: None,
                action: Action::Home,
            },
        )
        .map_err(|error| format!("{error:?}"))?;
    let home_observed = home.receipt.accepted
        && settle_foreground(&controller, &key, "com.apple.springboard").is_ok();
    controller
        .release(owner, lease)
        .map_err(|error| format!("{error:?}"))?;
    println!(
        "home_receipt={} home_observed={home_observed}",
        home.receipt.accepted
    );
    if !home_observed {
        return Err("Home effect unconfirmed".into());
    }
    Ok(())
}

fn home(expected: HardwareId) -> Result<(), String> {
    let (controller, key) = controller(expected)?;
    let before = controller
        .observe(&key)
        .map_err(|error| format!("{error:?}"))?;
    if before.state.lock != LockState::Unlocked
        || before.state.foreground_app.as_deref() != Some("com.apple.Preferences")
    {
        return Err("Home trial requires unlocked Settings in foreground".into());
    }
    let owner = "local-home-trial";
    let lease = controller
        .acquire(owner.into(), Duration::from_secs(30))
        .map_err(|error| format!("{error:?}"))?;
    let result = controller
        .act(
            owner,
            lease,
            ActionRequest {
                device: key.clone(),
                expected_revision: None,
                action: Action::Home,
            },
        )
        .map_err(|error| format!("{error:?}"))?;
    if !result.receipt.accepted {
        return Err("Home request unaccepted".into());
    }
    settle_foreground(&controller, &key, "com.apple.springboard")?;
    controller
        .release(owner, lease)
        .map_err(|error| format!("{error:?}"))?;
    println!("home_receipt=true home_observed=true");
    Ok(())
}

fn settle_foreground(
    controller: &Controller<WdaAdapter>,
    key: &DeviceKey,
    expected: &str,
) -> Result<Observation, String> {
    for _ in 0..12 {
        let observed = controller
            .observe(key)
            .map_err(|error| format!("{error:?}"))?;
        if observed.state.foreground_app.as_deref() == Some(expected) {
            return Ok(observed);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err("foreground effect unconfirmed within bounded settle".into())
}
