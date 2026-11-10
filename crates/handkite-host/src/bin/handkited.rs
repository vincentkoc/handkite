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

#[path = "handkited/signals.rs"]
mod signals;

const USAGE: &str = "usage: handkited | handkited --owned-wda | handkited --check-service-layout | handkited --check --config FILE | handkited --config SERVICE_CONFIG --socket-dir SERVICE_RUNTIME";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DaemonMode {
    Borrowed,
    Owned,
}

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
    serve: impl FnOnce(ServiceLayout, DaemonMode) -> Result<(), String>,
    output: impl Write,
) -> Result<u8, String> {
    run_with_layout_check(args, inspect, serve, check_service_layout, output)
}

fn run_with_layout_check(
    args: &[OsString],
    inspect: impl FnOnce(HardwareId) -> device_health::DeviceHealth,
    serve: impl FnOnce(ServiceLayout, DaemonMode) -> Result<(), String>,
    check_layout: impl FnOnce(&ServiceLayout) -> Result<(), String>,
    mut output: impl Write,
) -> Result<u8, String> {
    let (mode, supplied_paths) = match args {
        [flag] if flag == "--check-service-layout" => {
            let layout =
                ServiceLayout::current_account().map_err(|_| "service layout unavailable")?;
            check_layout(&layout)?;
            writeln!(output, "service_layout=ready").map_err(|_| "cannot write layout result")?;
            return Ok(0);
        }
        [check, flag, file] if check == "--check" && flag == "--config" => {
            let binding = host_config::load(Path::new(file)).map_err(str::to_owned)?;
            let health = inspect(binding.usb_id);
            serde_json::to_writer(&mut output, &health)
                .map_err(|_| "cannot write health result")?;
            writeln!(output).map_err(|_| "cannot write health result")?;
            return Ok(u8::from(!health.observation_ready()));
        }
        [] => (DaemonMode::Borrowed, None),
        [flag] if flag == "--owned-wda" => (DaemonMode::Owned, None),
        [flag, file, socket, directory] if flag == "--config" && socket == "--socket-dir" => (
            DaemonMode::Borrowed,
            Some((Path::new(file), Path::new(directory))),
        ),
        _ => return Err(USAGE.into()),
    };
    let layout = ServiceLayout::current_account().map_err(|_| "service layout unavailable")?;
    if let Some((config, runtime)) = supplied_paths
        && (config != layout.config_file() || runtime != layout.runtime_directory())
    {
        return Err("daemon requires the fixed service configuration and runtime paths".into());
    }
    serve(layout, mode)?;
    Ok(0)
}

fn check_service_layout(layout: &ServiceLayout) -> Result<(), String> {
    layout
        .validate_existing_directories()
        .map_err(|_| "unsafe service layout")?;
    let config = install_config::load_protected_config(layout)
        .map_err(|_| "protected service configuration unavailable")?;
    if config.wda_runner_bundle_id().is_none() {
        return Err("owned service runner is unconfigured".into());
    }
    Ok(())
}

fn serve(layout: ServiceLayout, mode: DaemonMode) -> Result<(), String> {
    let _signals = signals::ShutdownSignals::install().map_err(str::to_owned)?;
    let owner = ServiceLock::acquire(layout).map_err(|_| "cannot acquire service ownership")?;
    if signals::requested() {
        return Ok(());
    }
    match mode {
        DaemonMode::Borrowed => {
            load_before_serving(owner.layout(), install_config::load_protected, |binding| {
                bind_before_start(
                    || {
                        local_socket::bind_service(&owner)
                            .map_err(|_| "cannot bind protected socket".into())
                    },
                    |listener| {
                        if signals::requested() {
                            return Ok(());
                        }
                        let adapter = WdaAdapter::new(binding.usb_id.clone())
                            .map_err(|_| "cannot initialize pinned WDA adapter")?;
                        serve_adapter(binding, adapter, listener)
                    },
                )
            })
        }
        DaemonMode::Owned => {
            let config =
                install_config::load_protected_config(owner.layout()).map_err(str::to_owned)?;
            let runner_id = config
                .wda_runner_bundle_id()
                .ok_or("owned WDA requires a protected runner ID")?;
            let binding = config.binding().clone();
            bind_before_start(
                || {
                    local_socket::bind_service(&owner)
                        .map_err(|_| "cannot bind protected socket".into())
                },
                |listener| {
                    if signals::requested() {
                        return Ok(());
                    }
                    let adapter = WdaAdapter::new_owned(
                        binding.usb_id.clone(),
                        runner_id,
                        signals::requested,
                    )
                    .map_err(|_| "cannot start owned WDA runner and session")?;
                    serve_adapter(binding, adapter, listener)
                },
            )
        }
    }
}

fn bind_before_start<T>(
    bind: impl FnOnce() -> Result<T, String>,
    start: impl FnOnce(T) -> Result<(), String>,
) -> Result<(), String> {
    start(bind()?)
}

fn serve_adapter(
    binding: DeviceBinding,
    adapter: WdaAdapter,
    listener: local_socket::ServiceListener<'_>,
) -> Result<(), String> {
    let key = binding.key.clone();
    let controller = Controller::new(binding, adapter, 5000, 5000);
    if signals::requested() {
        return Ok(());
    }
    controller
        .observe(&key)
        .map_err(|_| "pinned device is not ready")?;
    if signals::requested() {
        return Ok(());
    }
    eprintln!("handkited ready");
    listener
        .serve_until(&controller, signals::requested)
        .map_err(|_| "local socket failed".into())
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
