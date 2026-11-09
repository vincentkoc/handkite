use handkite_host::{device_health, host_config};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 || args[1] != "--config" {
        eprintln!("usage: handkite-doctor --config FILE");
        std::process::exit(2);
    }
    let binding = match host_config::load(Path::new(&args[2])) {
        Ok(binding) => binding,
        Err(error) => {
            eprintln!("handkite-doctor: {error}");
            std::process::exit(2);
        }
    };
    let health = device_health::inspect(binding.usb_id);
    match serde_json::to_string(&health) {
        Ok(output) => println!("{output}"),
        Err(_) => {
            eprintln!("handkite-doctor: cannot encode health result");
            std::process::exit(2);
        }
    }
    if !health.observation_ready() {
        std::process::exit(1);
    }
}
