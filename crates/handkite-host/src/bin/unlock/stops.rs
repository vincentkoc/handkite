//! Private receipt IO only; never invoke an unlock, device or mutable gate API.
//!
//! `--stop-state-snapshot > private.json` captures the current disabled state.
//! `--check-stop-state < private.json` checks a previously reviewed receipt.
//! The broker must retain its independent device/service ownership checks and
//! explicit attempt authority. Never automatically replace a mismatched receipt.

use handkite_host::unlock_state::stop_snapshot;
use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;

fn mode(args: &[String]) -> Result<bool, &'static str> {
    match args {
        [_, flag] if flag == "--stop-state-snapshot" => Ok(true),
        [_, flag] if flag == "--check-stop-state" => Ok(false),
        _ => Err("invalid_arguments"),
    }
}

pub(super) fn run(args: &[String]) -> Result<(), String> {
    if mode(args)? {
        let receipt = stop_snapshot::capture()?;
        std::io::stdout()
            .lock()
            .write_all(&receipt)
            .map_err(|_| "stop_snapshot_output")?;
    } else {
        let descriptor = unsafe { libc::dup(libc::STDIN_FILENO) };
        if descriptor < 0 {
            return Err("stop_snapshot_invalid".into());
        }
        // Own only the duplicate, leaving the process's private input descriptor intact.
        let mut input = unsafe { File::from_raw_fd(descriptor) };
        stop_snapshot::check(&mut input)?;
        println!("stop_snapshot=match");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_modes_accept_no_device_config_or_action_flags() {
        for flag in ["--stop-state-snapshot", "--check-stop-state"] {
            let mut args = vec!["unlock".into(), flag.into()];
            assert_eq!(mode(&args), Ok(flag == "--stop-state-snapshot"));
            for extra in ["--config", "--owned-wda-session", "--state-dir", "--retry"] {
                args.push(extra.into());
                assert_eq!(mode(&args), Err("invalid_arguments"));
                args.pop();
            }
        }
    }
}
