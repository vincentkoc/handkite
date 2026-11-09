use base64::Engine;
use handkite_core::protocol::HardwareId;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_transport::UsbWda;
use std::time::{Duration, Instant};

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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime");
    let command = std::env::args().nth(1).unwrap_or_else(|| "status".into());
    let wda = UsbWda::new(PinnedUsb::new(expected));
    let result = runtime.block_on(async {
        match command.as_str() {
            "status" => status(&wda).await,
            "observe" => observe(&wda).await,
            _ => Err("unknown probe command".into()),
        }
    });
    match result {
        Ok(()) => {}
        Err(error) => {
            eprintln!("probe_error={error}");
            std::process::exit(1);
        }
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

async fn observe(wda: &UsbWda) -> Result<(), String> {
    let start = Instant::now();
    let screenshot = wda
        .request("GET", "/screenshot", b"", Duration::from_secs(20))
        .await
        .map_err(|error| format!("{error:?}"))?;
    if screenshot.status != 200 {
        return Err("screenshot request failed".into());
    }
    let screenshot: serde_json::Value =
        serde_json::from_slice(&screenshot.body).map_err(|_| "invalid screenshot response")?;
    let encoded = screenshot["value"]
        .as_str()
        .ok_or("screenshot is missing")?;
    let png = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "screenshot is not base64")?;
    if !png.starts_with(b"\x89PNG\r\n\x1a\n") || png.len() < 24 {
        return Err("screenshot is not PNG".into());
    }
    let width = u32::from_be_bytes(png[16..20].try_into().map_err(|_| "invalid PNG width")?);
    let height = u32::from_be_bytes(png[20..24].try_into().map_err(|_| "invalid PNG height")?);

    let source = wda
        .request("GET", "/source", b"", Duration::from_secs(20))
        .await
        .map_err(|error| format!("{error:?}"))?;
    if source.status != 200 {
        return Err("accessibility request failed".into());
    }
    let source: serde_json::Value =
        serde_json::from_slice(&source.body).map_err(|_| "invalid accessibility response")?;
    let tree = source["value"]
        .as_str()
        .ok_or("accessibility tree is missing")?;
    println!(
        "screenshot_png={}x{} source_nonempty={} window_ms={}",
        width,
        height,
        !tree.is_empty(),
        start.elapsed().as_millis()
    );
    Ok(())
}
