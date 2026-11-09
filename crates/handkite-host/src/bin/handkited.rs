use handkite_core::controller::Controller;
use handkite_core::protocol::{DeviceBinding, HardwareId};
use handkite_host::service_layout::ServiceLayout;
use handkite_host::service_lock::ServiceLock;
use handkite_host::wda_adapter::WdaAdapter;
use handkite_host::{device_health, host_config, install_config, local_socket};
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: handkited | handkited --check --config FILE | handkited --config SERVICE_CONFIG --socket-dir SERVICE_RUNTIME";

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
    serve: impl FnOnce(ServiceLayout) -> Result<(), String>,
    mut output: impl Write,
) -> Result<u8, String> {
    let supplied_paths = match args {
        [check, flag, file] if check == "--check" && flag == "--config" => {
            let binding = host_config::load(Path::new(file)).map_err(str::to_owned)?;
            let health = inspect(binding.usb_id);
            serde_json::to_writer(&mut output, &health)
                .map_err(|_| "cannot write health result")?;
            writeln!(output).map_err(|_| "cannot write health result")?;
            return Ok(u8::from(!health.observation_ready()));
        }
        [] => None,
        [flag, file, socket, directory] if flag == "--config" && socket == "--socket-dir" => {
            Some((Path::new(file), Path::new(directory)))
        }
        _ => return Err(USAGE.into()),
    };
    let layout = ServiceLayout::current_account().map_err(|_| "service layout unavailable")?;
    if let Some((config, runtime)) = supplied_paths {
        if config != layout.config_file() || runtime != layout.runtime_directory() {
            return Err("daemon requires the fixed service configuration and runtime paths".into());
        }
    }
    serve(layout)?;
    Ok(0)
}

fn serve(layout: ServiceLayout) -> Result<(), String> {
    let owner = ServiceLock::acquire(layout).map_err(|_| "cannot acquire service ownership")?;
    load_before_serving(owner.layout(), install_config::load_protected, |binding| {
        let listener =
            local_socket::bind_service(&owner).map_err(|_| "cannot bind protected socket")?;
        let key = binding.key.clone();
        let adapter = WdaAdapter::new(binding.usb_id.clone())
            .map_err(|_| "cannot initialize pinned WDA adapter")?;
        let controller = Controller::new(binding, adapter, 5000, 5000);
        controller
            .observe(&key)
            .map_err(|_| "pinned device is not ready")?;
        eprintln!("handkited ready");
        listener
            .serve(&controller)
            .map_err(|_| "local socket failed".into())
    })
}

fn load_before_serving(
    layout: &ServiceLayout,
    load: impl FnOnce(&ServiceLayout) -> Result<DeviceBinding, &'static str>,
    serve: impl FnOnce(DeviceBinding) -> Result<(), String>,
) -> Result<(), String> {
    let binding = load(layout).map_err(str::to_owned)?;
    serve(binding)
}

#[cfg(test)]
#[path = "handkited/tests.rs"]
mod tests;
