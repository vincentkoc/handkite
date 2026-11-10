use handkite_core::unlock::{AttemptGate, UnlockOutcome, UnlockPolicy, unlock_once};
use handkite_host::host_config;
use handkite_host::pin_backend::NativePasscodeBackend;
use handkite_host::pin_input;
use handkite_host::unlock_state;
use std::path::Path;

#[path = "unlock/idle.rs"]
mod idle;

fn main() {
    if let Err(error) = run() {
        eprintln!("handkite-unlock-once: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let (
        supervised_visibility_fallback,
        temporary_idle_settings,
        coordinate_pin_taps,
        owned_session,
    ) = flags(&args)?;
    if args[1] != "--config" {
        return Err(
            "usage: handkite-unlock-once --config FILE [--supervised-visibility-fallback] [--temporary-idle-settings] [--coordinate-pin-taps] [--owned-wda-session]".into(),
        );
    }
    let binding = host_config::load(Path::new(&args[2])).map_err(str::to_owned)?;
    if binding.key.as_str() != "iphone" {
        return Err("PIN entry requires the fixed device key iphone".into());
    }
    let mut gate =
        unlock_state::iphone_gate().map_err(|_| "account PIN state is unavailable or insecure")?;
    if gate
        .is_disabled()
        .map_err(|_| "cannot read PIN stop marker")?
    {
        return Err("PIN attempts disabled; owner inspection required".into());
    }
    if temporary_idle_settings {
        return idle::run(
            binding,
            gate,
            supervised_visibility_fallback,
            coordinate_pin_taps,
            owned_session,
        );
    }
    let code = pin_input::read_once(std::io::stdin().lock()).map_err(str::to_owned)?;
    let mut backend = NativePasscodeBackend::new_with_supervised_visibility_fallback(
        binding.usb_id.clone(),
        supervised_visibility_fallback,
    )
    .map_err(|_| "cannot initialize passcode backend")?;
    let outcome = unlock_once(
        &mut backend,
        &mut gate,
        &binding,
        &binding.key,
        &code,
        UnlockPolicy::default(),
    )
    .map_err(|error| format!("unlock_error={error:?}"))?;
    println!(
        "unlock={}",
        match outcome {
            UnlockOutcome::CodeSubmitted => "code_submitted",
            UnlockOutcome::AlreadyUnlocked => "already_unlocked",
        }
    );
    Ok(())
}

fn flags(args: &[String]) -> Result<(bool, bool, bool, bool), String> {
    if args.len() < 3 {
        return Err("invalid_arguments".into());
    }
    let mut supervised = false;
    let mut temporary = false;
    let mut coordinate = false;
    let mut owned = false;
    for arg in &args[3..] {
        match arg.as_str() {
            "--supervised-visibility-fallback" if !supervised => supervised = true,
            "--temporary-idle-settings" if !temporary => temporary = true,
            "--coordinate-pin-taps" if !coordinate => coordinate = true,
            "--owned-wda-session" if !owned => owned = true,
            _ => return Err("invalid_arguments".into()),
        }
    }
    if (coordinate || owned) && !(supervised && temporary) {
        return Err("invalid_arguments".into());
    }
    Ok((supervised, temporary, coordinate, owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_settings_are_explicit_and_duplicate_flags_refuse() {
        let base = vec!["unlock".into(), "--config".into(), "private".into()];
        assert_eq!(flags(&base), Ok((false, false, false, false)));
        let mut args = base;
        args.push("--temporary-idle-settings".into());
        assert_eq!(flags(&args), Ok((false, true, false, false)));
        args.push("--supervised-visibility-fallback".into());
        assert_eq!(flags(&args), Ok((true, true, false, false)));
        args.push("--temporary-idle-settings".into());
        assert!(flags(&args).is_err());
        assert!(flags(&[]).is_err());
    }

    #[test]
    fn coordinate_flag_requires_both_opt_ins_and_refuses_duplicates() {
        for supervised in [false, true] {
            for temporary in [false, true] {
                let mut args = vec![
                    "unlock".into(),
                    "--config".into(),
                    "private".into(),
                    "--coordinate-pin-taps".into(),
                ];
                if supervised {
                    args.push("--supervised-visibility-fallback".into());
                }
                if temporary {
                    args.push("--temporary-idle-settings".into());
                }
                assert_eq!(flags(&args).is_ok(), supervised && temporary);
                if supervised && temporary {
                    assert_eq!(flags(&args), Ok((true, true, true, false)));
                }
                args.push("--coordinate-pin-taps".into());
                assert_eq!(flags(&args), Err("invalid_arguments".into()));
            }
        }
    }

    #[test]
    fn owned_session_requires_both_opt_ins_and_refuses_duplicate_or_unknown_flags() {
        for supervised in [false, true] {
            for temporary in [false, true] {
                let mut args = vec![
                    "unlock".into(),
                    "--config".into(),
                    "private".into(),
                    "--owned-wda-session".into(),
                ];
                if supervised {
                    args.push("--supervised-visibility-fallback".into());
                }
                if temporary {
                    args.push("--temporary-idle-settings".into());
                }
                assert_eq!(flags(&args).is_ok(), supervised && temporary);
                if supervised && temporary {
                    assert_eq!(flags(&args), Ok((true, true, false, true)));
                }
                args.push("--owned-wda-session".into());
                assert!(flags(&args).is_err());
            }
        }
        assert!(
            flags(&[
                "unlock".into(),
                "--config".into(),
                "private".into(),
                "--renew-session".into()
            ])
            .is_err()
        );
    }
}
