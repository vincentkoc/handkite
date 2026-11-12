use handkite_core::protocol::DeviceKey;
use handkite_host::service_layout::ServiceLayout;
use handkite_host::{install_config, local_client, mcp};
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: handkite-mcp --service [--capabilities observation|navigation|observation,navigation|observation,navigation,interaction] | handkite-mcp --socket PATH --device DEVICE_KEY [--capabilities observation|navigation|observation,navigation|observation,navigation,interaction]";

enum Endpoint<'a> {
    Service,
    Explicit { socket: &'a str, device: &'a str },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("handkite-mcp: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let (endpoint, capabilities) = parse_args(&args[1..])?;
    let (socket, device) = resolve_endpoint(endpoint)?;
    mcp::serve_with_capabilities(
        BufReader::new(io::stdin().lock()),
        BufWriter::new(io::stdout().lock()),
        &device,
        capabilities,
        |request| local_client::exchange(&socket, request),
    )
    .map_err(|_| "MCP transport failed".into())
}

fn parse_args(args: &[String]) -> Result<(Endpoint<'_>, mcp::Capabilities), String> {
    let (endpoint, rest) = match args {
        [flag, rest @ ..] if flag == "--service" => (Endpoint::Service, rest),
        [socket_flag, socket, device_flag, device, rest @ ..]
            if socket_flag == "--socket" && device_flag == "--device" =>
        {
            (Endpoint::Explicit { socket, device }, rest)
        }
        _ => return Err(USAGE.into()),
    };
    Ok((endpoint, parse_capabilities(rest)?))
}

fn resolve_endpoint(endpoint: Endpoint<'_>) -> Result<(PathBuf, DeviceKey), String> {
    match endpoint {
        Endpoint::Service => {
            let layout =
                ServiceLayout::current_account().map_err(|_| "service layout unavailable")?;
            let config = install_config::load_protected_config(&layout)
                .map_err(|_| "protected service configuration unavailable")?;
            Ok((layout.socket(), config.binding().key.clone()))
        }
        Endpoint::Explicit { socket, device } => {
            let path = Path::new(socket);
            if !path.is_absolute() {
                return Err("socket path must be absolute".into());
            }
            let key = DeviceKey::new(device).map_err(|_| "invalid device key")?;
            Ok((path.to_path_buf(), key))
        }
    }
}

fn parse_capabilities(args: &[String]) -> Result<mcp::Capabilities, String> {
    match args {
        [] => Ok(mcp::Capabilities::Observation),
        [flag, value] if flag == "--capabilities" => match value.as_str() {
            "observation" => Ok(mcp::Capabilities::Observation),
            "navigation" => Ok(mcp::Capabilities::Navigation),
            "observation,navigation" => Ok(mcp::Capabilities::ObservationAndNavigation),
            "observation,navigation,interaction" => {
                Ok(mcp::Capabilities::ObservationNavigationInteraction)
            }
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
            ("observation,navigation,interaction", 3),
        ] {
            let parsed = parse_capabilities(&["--capabilities".into(), value.into()]).unwrap();
            assert_eq!(
                match parsed {
                    mcp::Capabilities::Observation => 0,
                    mcp::Capabilities::Navigation => 1,
                    mcp::Capabilities::ObservationAndNavigation => 2,
                    mcp::Capabilities::ObservationNavigationInteraction => 3,
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

    #[test]
    fn service_mode_has_no_client_selected_endpoint() {
        for (args, allowed) in [
            (vec!["--service"], true),
            (
                vec!["--service", "--capabilities", "observation,navigation"],
                true,
            ),
            (vec!["--service", "--socket", "/tmp/other"], false),
            (vec!["--service", "--device", "other"], false),
            (
                vec![
                    "--service",
                    "--capabilities",
                    "navigation",
                    "--device",
                    "other",
                ],
                false,
            ),
        ] {
            let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
            assert_eq!(parse_args(&args).is_ok(), allowed);
        }
    }

    #[test]
    fn explicit_mode_requires_a_fixed_argument_order_and_absolute_socket() {
        let args = ["--socket", "/tmp/fixture.sock", "--device", "phone"].map(str::to_owned);
        let (endpoint, capabilities) = parse_args(&args).unwrap();
        assert!(matches!(capabilities, mcp::Capabilities::Observation));
        let (socket, device) = resolve_endpoint(endpoint).unwrap();
        assert_eq!(socket, Path::new("/tmp/fixture.sock"));
        assert_eq!(device.as_str(), "phone");
        let relative = ["--socket", "relative.sock", "--device", "phone"].map(str::to_owned);
        assert!(resolve_endpoint(parse_args(&relative).unwrap().0).is_err());
    }
}
