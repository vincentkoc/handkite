use handkite_core::protocol::DeviceKey;
use handkite_host::pacing::{Pacer, Policy};
use handkite_host::service_layout::ServiceLayout;
use handkite_host::{install_config, local_client, mcp};
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: handkite-mcp --service [OPTIONS] | handkite-mcp --socket PATH --device DEVICE_KEY [OPTIONS]; OPTIONS: [--capabilities observation|navigation|observation,navigation|observation,navigation,interaction] [--pacing off|natural|cautious] [--pacing-seed N]";

/// Session options after the endpoint, each at most once.
struct Options {
    capabilities: mcp::Capabilities,
    pacing: Policy,
    seed: Option<u64>,
}

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
    let (endpoint, options) = parse_args(&args[1..])?;
    let (socket, device) = resolve_endpoint(endpoint)?;
    let seed = options.seed.unwrap_or_else(default_seed);
    mcp::serve_with_pacing(
        BufReader::new(io::stdin().lock()),
        BufWriter::new(io::stdout().lock()),
        &device,
        options.capabilities,
        Pacer::new(options.pacing, seed),
        |request| local_client::exchange(&socket, request),
    )
    .map_err(|_| "MCP transport failed".into())
}

/// Unseeded sessions still vary between runs; pass --pacing-seed to replay.
fn default_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 32)
}

fn parse_args(args: &[String]) -> Result<(Endpoint<'_>, Options), String> {
    let (endpoint, rest) = match args {
        [flag, rest @ ..] if flag == "--service" => (Endpoint::Service, rest),
        [socket_flag, socket, device_flag, device, rest @ ..]
            if socket_flag == "--socket" && device_flag == "--device" =>
        {
            (Endpoint::Explicit { socket, device }, rest)
        }
        _ => return Err(USAGE.into()),
    };
    Ok((endpoint, parse_options(rest)?))
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut capabilities = None;
    let mut pacing = None;
    let mut seed = None;
    let mut rest = args;
    while !rest.is_empty() {
        let [flag, value, tail @ ..] = rest else {
            return Err("invalid capability option".into());
        };
        match flag.as_str() {
            "--capabilities" if capabilities.is_none() => {
                capabilities = Some(parse_capabilities(&rest[..2])?);
            }
            "--pacing" if pacing.is_none() => {
                pacing = Some(Policy::parse(value).ok_or("invalid pacing policy")?);
            }
            "--pacing-seed" if seed.is_none() => {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err("invalid pacing seed".into());
                }
                seed = Some(value.parse().map_err(|_| "invalid pacing seed")?);
            }
            _ => return Err("invalid capability option".into()),
        }
        rest = tail;
    }
    Ok(Options {
        capabilities: capabilities.unwrap_or(mcp::Capabilities::Observation),
        pacing: pacing.unwrap_or_default(),
        seed,
    })
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
    fn pacing_is_a_launch_option_that_defaults_off() {
        let parse = |args: &[&str]| {
            parse_options(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
        };
        let options = parse(&[]).unwrap();
        assert_eq!((options.pacing, options.seed), (Policy::Off, None));
        let options = parse(&[
            "--pacing",
            "natural",
            "--capabilities",
            "observation,navigation,interaction",
            "--pacing-seed",
            "42",
        ])
        .unwrap();
        assert_eq!((options.pacing, options.seed), (Policy::Natural, Some(42)));
        assert!(matches!(
            options.capabilities,
            mcp::Capabilities::ObservationNavigationInteraction
        ));
        assert_eq!(
            parse(&["--pacing", "cautious"]).unwrap().pacing,
            Policy::Cautious
        );
        for bad in [
            &["--pacing"][..],
            &["--pacing", "fast"],
            &["--pacing", "off", "--pacing", "natural"],
            &["--pacing-seed", "-1"],
            &["--pacing-seed", "+1"],
            &["--pacing-seed", "18446744073709551616"],
            &[
                "--capabilities",
                "navigation",
                "--capabilities",
                "navigation",
            ],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
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
        let (endpoint, options) = parse_args(&args).unwrap();
        assert!(matches!(
            options.capabilities,
            mcp::Capabilities::Observation
        ));
        assert_eq!(options.pacing, Policy::Off);
        let (socket, device) = resolve_endpoint(endpoint).unwrap();
        assert_eq!(socket, Path::new("/tmp/fixture.sock"));
        assert_eq!(device.as_str(), "phone");
        let relative = ["--socket", "relative.sock", "--device", "phone"].map(str::to_owned);
        assert!(resolve_endpoint(parse_args(&relative).unwrap().0).is_err());
    }
}
