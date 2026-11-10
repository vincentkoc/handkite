use handkite_host::{
    device_agreement::DeviceAgreement,
    install_config::{self, RetainedConfig},
    installer_lock::InstallerLock,
    service_layout::ServiceLayout,
};
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pass,
    Reject,
}

fn main() -> ExitCode {
    let outcome = run(std::env::args_os().skip(1), observe_service);
    ExitCode::from(report(outcome, &mut io::stdout().lock()))
}

fn run(
    mut args: impl Iterator<Item = OsString>,
    observe: impl FnOnce() -> Result<(), &'static str>,
) -> Outcome {
    if args.next().as_deref() != Some(OsStr::new("--service")) || args.next().is_some() {
        return Outcome::Reject;
    }
    match observe() {
        Ok(()) => Outcome::Pass,
        Err(_) => Outcome::Reject,
    }
}

fn observe_service() -> Result<(), &'static str> {
    let layout = ServiceLayout::current_account().map_err(|_| "service layout unavailable")?;
    // This binding is self-loaded, not an independently supplied installer pin.
    // Read it before lock acquisition, which can create the fixed lock file.
    let expected = install_config::load_protected(&layout)?;
    let owner = InstallerLock::acquire(&layout)?;
    let mut config = RetainedConfig::acquire(&owner, &expected)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "runtime unavailable")?;
    runtime.block_on(async {
        DeviceAgreement::observe(&owner, &mut config)
            .await
            .map(|_| ())
    })
}

fn report(outcome: Outcome, output: &mut impl Write) -> u8 {
    let (code, bytes): (u8, &[u8]) = match outcome {
        Outcome::Pass => (0, b"device_agreement=pass\n"),
        Outcome::Reject => (1, b"device_agreement=reject\n"),
    };
    if output.write_all(bytes).is_err() {
        return 1;
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn args(values: &[&str]) -> impl Iterator<Item = OsString> {
        values.iter().map(OsString::from)
    }

    #[test]
    fn invalid_arguments_never_enter_service_setup() {
        for values in [
            vec![],
            vec![""],
            vec!["--help"],
            vec!["--service=true"],
            vec!["--Service"],
            vec![" --service"],
            vec!["--service", ""],
            vec!["--service", "--service"],
            vec!["--service", "--config", "/fixture/config"],
            vec!["--service", "--device", "fixture-device"],
            vec!["--service", "--socket", "/fixture/socket"],
            vec!["--service", "--root", "/fixture"],
            vec!["--", "--service"],
        ] {
            assert_eq!(
                run(args(&values), || panic!("unexpected service setup")),
                Outcome::Reject
            );
        }
        for values in [
            vec![OsString::from_vec(b"--service\xff".to_vec())],
            vec!["--service".into(), OsString::from_vec(vec![0xff])],
        ] {
            assert_eq!(
                run(values.into_iter(), || panic!("unexpected service setup")),
                Outcome::Reject
            );
        }
    }

    #[test]
    fn exact_service_form_calls_continuation_once() {
        let mut calls = 0;
        let outcome = run(args(&["--service"]), || {
            calls += 1;
            Ok(())
        });
        assert_eq!(calls, 1);
        let mut output = Vec::new();
        assert_eq!(report(outcome, &mut output), 0);
        assert_eq!(output, b"device_agreement=pass\n");
    }

    #[test]
    fn failed_continuation_is_not_retried_or_printed() {
        let mut calls = 0;
        let outcome = run(args(&["--service"]), || {
            calls += 1;
            Err("private fixture identity and transport detail")
        });
        assert_eq!(calls, 1);
        let mut output = Vec::new();
        assert_eq!(report(outcome, &mut output), 1);
        assert_eq!(output, b"device_agreement=reject\n");
    }

    #[test]
    fn output_failure_exits_nonzero() {
        for outcome in [Outcome::Pass, Outcome::Reject] {
            assert_eq!(report(outcome, &mut io::Cursor::new([0; 0])), 1);
        }
    }
}
