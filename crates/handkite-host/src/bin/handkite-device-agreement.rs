use handkite_host::{
    device_agreement::{DeviceAgreement, ObservationFailure},
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
    Reject(FailureStage),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureStage {
    Arguments,
    Layout,
    ConfigLoad,
    Lock,
    ConfigRetain,
    Runtime,
    Observation(ObservationFailure),
}

impl FailureStage {
    fn label(self) -> &'static str {
        match self {
            Self::Arguments => "arguments",
            Self::Layout => "service_layout",
            Self::ConfigLoad => "config_load",
            Self::Lock => "installer_lock",
            Self::ConfigRetain => "config_retain",
            Self::Runtime => "runtime",
            Self::Observation(failure) => failure.stage(),
        }
    }
}

fn main() -> ExitCode {
    let outcome = run(std::env::args_os().skip(1), observe_service);
    ExitCode::from(report(outcome, &mut io::stdout().lock()))
}

fn run(
    mut args: impl Iterator<Item = OsString>,
    observe: impl FnOnce() -> Result<(), FailureStage>,
) -> Outcome {
    if args.next().as_deref() != Some(OsStr::new("--service")) || args.next().is_some() {
        return Outcome::Reject(FailureStage::Arguments);
    }
    match observe() {
        Ok(()) => Outcome::Pass,
        Err(stage) => Outcome::Reject(stage),
    }
}

fn observe_service() -> Result<(), FailureStage> {
    let layout = ServiceLayout::current_account().map_err(|_| FailureStage::Layout)?;
    // This binding is self-loaded, not an independently supplied installer pin.
    // Read it before lock acquisition, which can create the fixed lock file.
    let expected = install_config::load_protected(&layout).map_err(|_| FailureStage::ConfigLoad)?;
    let owner = InstallerLock::acquire(&layout).map_err(|_| FailureStage::Lock)?;
    let mut config =
        RetainedConfig::acquire(&owner, &expected).map_err(|_| FailureStage::ConfigRetain)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| FailureStage::Runtime)?;
    runtime.block_on(async {
        DeviceAgreement::observe_diagnostic(&owner, &mut config)
            .await
            .map(|_| ())
            .map_err(FailureStage::Observation)
    })
}

fn report(outcome: Outcome, output: &mut impl Write) -> u8 {
    let (code, result) = match outcome {
        Outcome::Pass => (0, output.write_all(b"device_agreement=pass\n")),
        Outcome::Reject(stage) => (
            1,
            writeln!(output, "device_agreement=reject stage={}", stage.label()),
        ),
    };
    if result.is_err() {
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
                Outcome::Reject(FailureStage::Arguments)
            );
        }
        for values in [
            vec![OsString::from_vec(b"--service\xff".to_vec())],
            vec!["--service".into(), OsString::from_vec(vec![0xff])],
        ] {
            assert_eq!(
                run(values.into_iter(), || panic!("unexpected service setup")),
                Outcome::Reject(FailureStage::Arguments)
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
    fn every_failure_has_only_its_fixed_stage_and_is_not_retried() {
        use ObservationFailure::*;
        for (stage, label) in [
            (FailureStage::Arguments, "arguments"),
            (FailureStage::Layout, "service_layout"),
            (FailureStage::ConfigLoad, "config_load"),
            (FailureStage::Lock, "installer_lock"),
            (FailureStage::ConfigRetain, "config_retain"),
            (FailureStage::Runtime, "runtime"),
            (
                FailureStage::Observation(UnsupportedPlatform),
                "observation_platform",
            ),
            (
                FailureStage::Observation(ConfigBefore),
                "config_revalidate_before",
            ),
            (FailureStage::Observation(UsbBefore), "usb_inventory_before"),
            (FailureStage::Observation(Route), "usb_route_agreement"),
            (
                FailureStage::Observation(LockdownConnect),
                "lockdown_connect",
            ),
            (FailureStage::Observation(LockdownQuery), "lockdown_query"),
            (
                FailureStage::Observation(Identity),
                "lockdown_identity_agreement",
            ),
            (FailureStage::Observation(UsbAfter), "usb_inventory_after"),
            (
                FailureStage::Observation(ConfigAfter),
                "config_revalidate_after",
            ),
            (FailureStage::Observation(Deadline), "observation_deadline"),
        ] {
            let mut calls = 0;
            let outcome = run(args(&["--service"]), || {
                calls += 1;
                Err(stage)
            });
            assert_eq!(calls, 1);
            let mut output = Vec::new();
            assert_eq!(report(outcome, &mut output), 1);
            assert_eq!(
                output,
                format!("device_agreement=reject stage={label}\n").as_bytes()
            );
        }
    }

    #[test]
    fn output_failure_exits_nonzero() {
        for outcome in [Outcome::Pass, Outcome::Reject(FailureStage::Arguments)] {
            assert_eq!(report(outcome, &mut io::Cursor::new([0; 0])), 1);
        }
    }
}
