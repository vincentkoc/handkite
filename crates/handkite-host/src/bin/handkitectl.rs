use handkite_core::protocol::DeviceKey;
use handkite_host::local_client;
use handkite_host::wire::Request;
use serde_json::json;
use std::io::{self, Read};
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("handkitectl: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 || args[1] != "--socket" {
        return Err("usage: handkitectl --socket PATH observe DEVICE_KEY | request".into());
    }
    let socket = Path::new(&args[2]);
    match (args[3].as_str(), args.len()) {
        ("observe", 5) => {
            let key = DeviceKey::new(&args[4]).map_err(|_| "invalid device key")?;
            let request = serde_json::to_vec(&json!({"method":"observe","device":key}))
                .map_err(|_| "cannot encode request")?;
            let response = local_client::exchange(socket, &request)
                .map_err(|_| "local socket request failed")?;
            let value: serde_json::Value =
                serde_json::from_slice(&response).map_err(|_| "invalid local response")?;
            if value["type"] != "observed" {
                return Err(format!(
                    "device error: {}",
                    value["code"].as_str().unwrap_or("unknown")
                ));
            }
            let observation = &value["observation"];
            println!(
                "revision={} lock={} wda_ready={} foreground_app={} tree_nonempty={} screenshot_base64_bytes={}",
                observation["revision"].as_u64().ok_or("missing revision")?,
                observation["state"]["lock"]
                    .as_str()
                    .ok_or("missing lock state")?,
                observation["state"]["wda_ready"]
                    .as_bool()
                    .ok_or("missing WDA state")?,
                observation["state"]["foreground_app"]
                    .as_str()
                    .unwrap_or("none"),
                observation["accessibility_tree"]
                    .as_str()
                    .is_some_and(|tree| !tree.is_empty()),
                observation["screenshot_base64"]
                    .as_str()
                    .ok_or("missing screenshot")?
                    .len(),
            );
        }
        ("request", 4) => {
            let mut input = Vec::new();
            io::stdin()
                .take(64 * 1024 + 1)
                .read_to_end(&mut input)
                .map_err(|_| "cannot read request")?;
            if input.len() > 64 * 1024 {
                return Err("request too large".into());
            }
            while matches!(input.last(), Some(b'\n' | b'\r' | b' ' | b'\t')) {
                input.pop();
            }
            let _: Request = serde_json::from_slice(&input).map_err(|_| "invalid request")?;
            let response = local_client::exchange(socket, &input)
                .map_err(|_| "local socket request failed")?;
            println!(
                "{}",
                String::from_utf8(response).map_err(|_| "invalid local response")?
            );
        }
        _ => return Err("usage: handkitectl --socket PATH observe DEVICE_KEY | request".into()),
    }
    Ok(())
}
