use handkite_core::controller::Controller;
use handkite_host::host_config;
use handkite_host::local_socket;
use handkite_host::wda_adapter::WdaAdapter;
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("handkited: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 || args[1] != "--config" || args[3] != "--socket-dir" {
        return Err("usage: handkited --config FILE --socket-dir DIRECTORY".into());
    }
    let binding = host_config::load(Path::new(&args[2])).map_err(str::to_owned)?;
    let key = binding.key.clone();
    let adapter = WdaAdapter::new(binding.usb_id.clone())
        .map_err(|_| "cannot initialize pinned WDA adapter")?;
    let controller = Controller::new(binding, adapter, 5000, 5000);
    controller
        .observe(&key)
        .map_err(|_| "pinned device is not ready")?;
    let listener = local_socket::bind_protected(Path::new(&args[4]))
        .map_err(|error| format!("cannot bind protected socket: {error}"))?;
    eprintln!("handkited ready");
    local_socket::serve(listener, &controller).map_err(|_| "local socket failed".into())
}
