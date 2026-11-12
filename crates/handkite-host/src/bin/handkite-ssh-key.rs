use handkite_host::mcp::Capabilities;
use handkite_host::service_layout::ServiceLayout;
use handkite_host::ssh_bridge::{self, PUBLIC_KEY_BYTES};
use std::ffi::OsString;
use std::io::{self, Read, Write};

const USAGE: &str = "usage: handkite-ssh-key --capabilities observation|navigation|observation,navigation|observation,navigation,interaction < bare-public-key";

fn main() {
    if let Err(error) = run() {
        eprintln!("handkite-ssh-key: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), &'static str> {
    let args: Vec<_> = std::env::args_os().skip(1).take(3).collect();
    let capabilities = parse_args(&args)?;
    let key = read_key(io::stdin().lock())?;
    let layout = ServiceLayout::current_account().map_err(|_| "service layout unavailable")?;
    let line = ssh_bridge::render(&layout, &key, capabilities)?;
    io::stdout()
        .lock()
        .write_all(line.as_bytes())
        .map_err(|_| "entry output failed")
}

fn parse_args(args: &[OsString]) -> Result<Capabilities, &'static str> {
    let [flag, value] = args else {
        return Err(USAGE);
    };
    if flag != "--capabilities" {
        return Err(USAGE);
    }
    match value.to_str() {
        Some("observation") => Ok(Capabilities::Observation),
        Some("navigation") => Ok(Capabilities::Navigation),
        Some("observation,navigation") => Ok(Capabilities::ObservationAndNavigation),
        Some("observation,navigation,interaction") => {
            Ok(Capabilities::ObservationNavigationInteraction)
        }
        _ => Err("invalid capabilities"),
    }
}

fn read_key(input: impl Read) -> Result<String, &'static str> {
    // One extra byte beyond the optional LF detects oversized input without
    // draining an unbounded stream. Normal stdin EOF still terminates the read.
    let mut bytes = Vec::with_capacity(PUBLIC_KEY_BYTES + 2);
    input
        .take((PUBLIC_KEY_BYTES + 2) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "public key input unavailable")?;
    if bytes.len() == PUBLIC_KEY_BYTES + 1 && bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.len() != PUBLIC_KEY_BYTES {
        return Err("invalid SSH public key");
    }
    String::from_utf8(bytes).map_err(|_| "invalid SSH public key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn capabilities_require_exact_explicit_arguments() {
        for (value, expected) in [
            ("observation", Capabilities::Observation),
            ("navigation", Capabilities::Navigation),
            (
                "observation,navigation",
                Capabilities::ObservationAndNavigation,
            ),
            (
                "observation,navigation,interaction",
                Capabilities::ObservationNavigationInteraction,
            ),
        ] {
            let result = parse_args(&["--capabilities".into(), value.into()]).unwrap();
            assert_eq!(
                std::mem::discriminant(&result),
                std::mem::discriminant(&expected)
            );
        }
        for args in [
            vec![],
            vec!["--capabilities".into()],
            vec!["--capabilities".into(), "all".into()],
            vec!["--capabilities".into(), "navigation,observation".into()],
            vec!["--service".into(), "observation".into()],
            vec![
                "--capabilities".into(),
                "observation".into(),
                "--service".into(),
            ],
            vec!["--capabilities".into(), OsString::from_vec(vec![0xff])],
            vec![OsString::from_vec(vec![0xff]), "observation".into()],
        ] {
            assert!(parse_args(&args).is_err());
        }
    }

    #[test]
    fn stdin_accepts_exact_bytes_with_at_most_one_final_lf() {
        let bytes = vec![b'a'; PUBLIC_KEY_BYTES];
        assert_eq!(read_key(bytes.as_slice()).unwrap().as_bytes(), bytes);
        let mut with_lf = bytes.clone();
        with_lf.push(b'\n');
        assert_eq!(read_key(with_lf.as_slice()).unwrap().as_bytes(), bytes);
        for invalid in [
            Vec::new(),
            bytes[..PUBLIC_KEY_BYTES - 1].to_vec(),
            [bytes.as_slice(), b"\r\n"].concat(),
            [bytes.as_slice(), b"\n\n"].concat(),
            [bytes.as_slice(), b"x"].concat(),
            vec![0xff; PUBLIC_KEY_BYTES],
        ] {
            assert_eq!(read_key(invalid.as_slice()), Err("invalid SSH public key"));
        }
    }

    #[test]
    fn oversized_stdin_stops_after_the_fixed_limit() {
        let mut input = Cursor::new(vec![b'x'; 8192]);
        assert_eq!(read_key(&mut input), Err("invalid SSH public key"));
        assert_eq!(input.position(), (PUBLIC_KEY_BYTES + 2) as u64);
    }

    #[test]
    fn input_errors_have_only_fixed_output() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("private input detail"))
            }
        }
        assert_eq!(read_key(Broken), Err("public key input unavailable"));
    }
}
