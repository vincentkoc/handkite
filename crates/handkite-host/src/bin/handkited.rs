use handkite_core::controller::Controller;
use handkite_core::protocol::{DeviceBinding, HardwareId};
use handkite_host::wda_adapter::WdaAdapter;
use handkite_host::{device_health, host_config, local_socket};
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str =
    "usage: handkited --check --config FILE | handkited --config FILE --socket-dir DIRECTORY";

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match run(
        &args,
        device_health::inspect,
        serve,
        std::io::stdout().lock(),
    ) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("handkited: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(
    args: &[OsString],
    inspect: impl FnOnce(HardwareId) -> device_health::DeviceHealth,
    serve: impl FnOnce(DeviceBinding, &Path) -> Result<(), String>,
    mut output: impl Write,
) -> Result<u8, String> {
    let (config, socket_dir) = match args {
        [check, flag, file] if check == "--check" && flag == "--config" => (Path::new(file), None),
        [flag, file, socket, directory] if flag == "--config" && socket == "--socket-dir" => {
            (Path::new(file), Some(Path::new(directory)))
        }
        _ => return Err(USAGE.into()),
    };
    let binding = host_config::load(config).map_err(str::to_owned)?;
    if let Some(directory) = socket_dir {
        serve(binding, directory)?;
        return Ok(0);
    }
    let health = inspect(binding.usb_id);
    serde_json::to_writer(&mut output, &health).map_err(|_| "cannot write health result")?;
    writeln!(output).map_err(|_| "cannot write health result")?;
    Ok(u8::from(!health.observation_ready()))
}

fn serve(binding: DeviceBinding, socket_dir: &Path) -> Result<(), String> {
    let key = binding.key.clone();
    let adapter = WdaAdapter::new(binding.usb_id.clone())
        .map_err(|_| "cannot initialize pinned WDA adapter")?;
    let controller = Controller::new(binding, adapter, 5000, 5000);
    controller
        .observe(&key)
        .map_err(|_| "pinned device is not ready")?;
    let listener = local_socket::bind_protected(socket_dir)
        .map_err(|error| format!("cannot bind protected socket: {error}"))?;
    eprintln!("handkited ready");
    local_socket::serve(listener, &controller).map_err(|_| "local socket failed".into())
}

#[cfg(test)]
#[path = "handkited/tests.rs"]
mod tests;
