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
    if !matches!(args.len(), 5 | 7) || args[1] != "--socket" || args[3] != "--device" {
        return Err("usage: handkite-mcp --socket PATH --device DEVICE_KEY [--capabilities observation|navigation|observation,navigation]".into());
    }
    let socket = Path::new(&args[2]);
    if !socket.is_absolute() {
        return Err("socket path must be absolute".into());
    }
    let device = DeviceKey::new(&args[4]).map_err(|_| "invalid device key")?;
    let capabilities = parse_capabilities(&args[5..])?;
    mcp::serve_with_capabilities(
        BufReader::new(io::stdin().lock()),
        BufWriter::new(io::stdout().lock()),
        &device,
        capabilities,
        |request| local_client::exchange(socket, request),
    )
    .map_err(|_| "MCP transport failed".into())
}

fn parse_capabilities(args: &[String]) -> Result<mcp::Capabilities, String> {
    match args {
        [] => Ok(mcp::Capabilities::Observation),
        [flag, value] if flag == "--capabilities" => match value.as_str() {
            "observation" => Ok(mcp::Capabilities::Observation),
            "navigation" => Ok(mcp::Capabilities::Navigation),
            "observation,navigation" => Ok(mcp::Capabilities::ObservationAndNavigation),
            _ => Err("invalid capabilities".into()),
        },
        _ => Err("invalid capability option".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_are_explicit_and_default_to_observation() {
        assert!(matches!(
            parse_capabilities(&[]).unwrap(),
            mcp::Capabilities::Observation
        ));
        for (value, expected) in [
            ("observation", 0),
            ("navigation", 1),
            ("observation,navigation", 2),
        ] {
            let parsed = parse_capabilities(&["--capabilities".into(), value.into()]).unwrap();
            assert_eq!(
                match parsed {
                    mcp::Capabilities::Observation => 0,
                    mcp::Capabilities::Navigation => 1,
                    mcp::Capabilities::ObservationAndNavigation => 2,
                },
                expected
            );
        }
        for value in [
            "",
            "all",
            "input",
            "navigation,observation",
            "observation,navigation,lock",
        ] {
            assert!(parse_capabilities(&["--capabilities".into(), value.into()]).is_err());
        }
        assert!(parse_capabilities(&["--device".into(), "navigation".into()]).is_err());
    }
}
