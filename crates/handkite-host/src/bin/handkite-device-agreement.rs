use handkite_host::{
    device_agreement::{
        DeviceAgreement, LockdownQueryFailure, ObservationFailure, PlistDecodeFailure,
    },
    install_config,
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| FailureStage::Runtime)?;
    runtime.block_on(async {
        DeviceAgreement::observe_diagnostic(&owner, &expected)
            .await
            .map(|_| ())
            .map_err(FailureStage::Observation)
    })
}

fn report(outcome: Outcome, output: &mut impl Write) -> u8 {
    let (code, result) = match outcome {
        Outcome::Pass => (0, output.write_all(b"device_agreement=pass\n")),
        Outcome::Reject(FailureStage::Observation(ObservationFailure::LockdownQuery(query))) => {
            (1, report_query(query, output))
        }
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

fn report_query(query: LockdownQueryFailure, output: &mut impl Write) -> io::Result<()> {
    write!(
        output,
        "device_agreement=reject stage=lockdown_query substage={}",
        query.substage()
    )?;
    if let LockdownQueryFailure::PlistDecode(decode) = query {
        write!(
            output,
            " format={} cause={}",
            decode.format(),
            decode.cause()
        )?;
        if let PlistDecodeFailure::XmlReplySchema(envelope) = decode {
            write!(output, " envelope={}", envelope.label())?;
        }
    }
    writeln!(output)
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
            (FailureStage::Observation(ConfigRetain), "config_retain"),
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
    fn query_failures_append_only_a_fixed_substage_without_retry() {
        use handkite_host::device_agreement::LockdownQueryFailure::*;
        for (query, label) in [
            (RequestWrite, "request_write"),
            (ResponseLength, "response_length"),
            (ResponseRead, "response_read"),
            (ReplyValidation, "reply_validation"),
        ] {
            let mut calls = 0;
            let failure = ObservationFailure::LockdownQuery(query);
            assert_eq!(failure.stage(), "lockdown_query");
            let outcome = run(args(&["--service"]), || {
                calls += 1;
                Err(FailureStage::Observation(failure))
            });
            assert_eq!(calls, 1);
            let mut output = Vec::new();
            assert_eq!(report(outcome, &mut output), 1);
            assert_eq!(
                output,
                format!("device_agreement=reject stage=lockdown_query substage={label}\n")
                    .as_bytes()
            );
            assert_eq!(
                report(
                    Outcome::Reject(FailureStage::Observation(failure)),
                    &mut io::Cursor::new([0; 0])
                ),
                1
            );
        }
    }

    #[test]
    fn output_failure_exits_nonzero() {
        for outcome in [Outcome::Pass, Outcome::Reject(FailureStage::Arguments)] {
            assert_eq!(report(outcome, &mut io::Cursor::new([0; 0])), 1);
        }
    }

    #[test]
    fn decode_failures_append_only_fixed_format_cause_and_envelope() {
        use handkite_host::device_agreement::ReplyEnvelope;
        let mut cases = vec![
            (
                PlistDecodeFailure::BinaryFormat,
                "format=binary cause=binary_format",
            ),
            (
                PlistDecodeFailure::OtherFormat,
                "format=other cause=non_xml_prefix",
            ),
            (
                PlistDecodeFailure::XmlPreflight,
                "format=xml cause=xml_preflight",
            ),
            (
                PlistDecodeFailure::XmlPlistParse,
                "format=xml cause=plist_syntax",
            ),
        ];
        cases.extend([
            (
                PlistDecodeFailure::XmlReplySchema(ReplyEnvelope::None),
                "format=xml cause=reply_schema envelope=none",
            ),
            (
                PlistDecodeFailure::XmlReplySchema(ReplyEnvelope::Error),
                "format=xml cause=reply_schema envelope=error",
            ),
            (
                PlistDecodeFailure::XmlReplySchema(ReplyEnvelope::Result),
                "format=xml cause=reply_schema envelope=result",
            ),
            (
                PlistDecodeFailure::XmlReplySchema(ReplyEnvelope::Other),
                "format=xml cause=reply_schema envelope=other",
            ),
            (
                PlistDecodeFailure::XmlReplySchema(ReplyEnvelope::ErrorResult),
                "format=xml cause=reply_schema envelope=error_result",
            ),
            (
                PlistDecodeFailure::XmlReplySchema(ReplyEnvelope::ErrorOther),
                "format=xml cause=reply_schema envelope=error_other",
            ),
            (
                PlistDecodeFailure::XmlReplySchema(ReplyEnvelope::ResultOther),
                "format=xml cause=reply_schema envelope=result_other",
            ),
            (
                PlistDecodeFailure::XmlReplySchema(ReplyEnvelope::ErrorResultOther),
                "format=xml cause=reply_schema envelope=error_result_other",
            ),
            (
                PlistDecodeFailure::XmlReplySchema(ReplyEnvelope::NotDictionary),
                "format=xml cause=reply_schema envelope=not_dictionary",
            ),
        ]);
        for (cause, labels) in cases {
            let query = LockdownQueryFailure::PlistDecode(cause);
            let stage = FailureStage::Observation(ObservationFailure::LockdownQuery(query));
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
                format!(
                    "device_agreement=reject stage=lockdown_query substage=plist_decode {labels}\n"
                )
                .as_bytes()
            );
            assert_eq!(
                report(Outcome::Reject(stage), &mut io::Cursor::new([0; 0])),
                1
            );
        }
    }
}
