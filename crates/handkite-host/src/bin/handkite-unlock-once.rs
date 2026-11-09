use handkite_core::unlock::{AttemptGate, UnlockOutcome, UnlockPolicy, unlock_once};
use handkite_host::host_config;
use handkite_host::pin_backend::NativePasscodeBackend;
use handkite_host::pin_input;
use handkite_host::unlock_state;
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("handkite-unlock-once: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let supervised_visibility_fallback =
        args.len() == 4 && args[3] == "--supervised-visibility-fallback";
    if !(args.len() == 3 || supervised_visibility_fallback) || args[1] != "--config" {
        return Err(
            "usage: handkite-unlock-once --config FILE [--supervised-visibility-fallback]".into(),
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
