use handkite_core::unlock::{AttemptGate, UnlockOutcome, UnlockPolicy, unlock_once};
use handkite_host::file_gate::FileAttemptGate;
use handkite_host::host_config;
use handkite_host::pin_backend::NativePasscodeBackend;
use handkite_host::pin_input;
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("handkite-unlock-once: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 || args[1] != "--config" || args[3] != "--state-dir" {
        return Err("usage: handkite-unlock-once --config FILE --state-dir DIRECTORY".into());
    }
    let binding = host_config::load(Path::new(&args[2])).map_err(str::to_owned)?;
    let mut gate = FileAttemptGate::new(Path::new(&args[4]).join("UNLOCK-DISABLED"))
        .map_err(|_| "state directory must be private and absolute")?;
    if gate
        .is_disabled()
        .map_err(|_| "cannot read PIN stop marker")?
    {
        return Err("PIN attempts disabled; owner inspection required".into());
    }
    let code = pin_input::read_once(std::io::stdin().lock()).map_err(str::to_owned)?;
    let mut backend = NativePasscodeBackend::new(binding.usb_id.clone())
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
