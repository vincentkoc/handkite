use handkite_core::controller::Controller;
use handkite_core::protocol::{DeviceBinding, DeviceKey, HardwareId};
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_adapter::WdaAdapter;
use handkite_host::wda_transport::UsbWda;
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
        "status" => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Tokio runtime");
            let wda = UsbWda::new(PinnedUsb::new(expected));
            runtime.block_on(status(&wda))
        }
        "observe" => observe(expected),
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

fn observe(expected: HardwareId) -> Result<(), String> {
    let key = DeviceKey::new("iphone-15-pro").map_err(|error| error.to_string())?;
    let adapter = WdaAdapter::new(expected.clone()).map_err(|_| "cannot initialize WDA reader")?;
    let controller = Controller::new(
        DeviceBinding {
            key: key.clone(),
            usb_id: expected,
        },
        adapter,
        5000,
        5000,
    );
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
