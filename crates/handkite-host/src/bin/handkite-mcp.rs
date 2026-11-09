use handkite_core::protocol::DeviceKey;
use handkite_host::{local_client, mcp};
use std::io::{self, BufReader, BufWriter};
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("handkite-mcp: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 || args[1] != "--socket" || args[3] != "--device" {
        return Err("usage: handkite-mcp --socket PATH --device DEVICE_KEY".into());
    }
    let socket = Path::new(&args[2]);
    if !socket.is_absolute() {
        return Err("socket path must be absolute".into());
    }
    let device = DeviceKey::new(&args[4]).map_err(|_| "invalid device key")?;
    mcp::serve(
        BufReader::new(io::stdin().lock()),
        BufWriter::new(io::stdout().lock()),
        &device,
        |request| local_client::exchange(socket, request),
    )
    .map_err(|_| "MCP transport failed".into())
}
