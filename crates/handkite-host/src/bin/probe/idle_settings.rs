use handkite_host::host_config;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_settings::{
    IdleTimeouts, KnownIdleRecovery, SettingsError, read_active_idle_settings, restore_known_idle,
};
use handkite_host::wda_transport::UsbWda;
use std::path::Path;

fn pinned_usb(args: &[String]) -> Result<PinnedUsb, &'static str> {
    let [flag, path] = args else {
        return Err("refused reason=invalid_arguments");
    };
    if flag != "--config" {
        return Err("refused reason=invalid_arguments");
    }
    let binding =
        host_config::load(Path::new(path)).map_err(|_| "refused reason=configuration_invalid")?;
    Ok(PinnedUsb::new(binding.usb_id))
}

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let usb = pinned_usb(args)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "refused reason=settings_unproven")?;
    let settings = runtime.block_on(async {
        usb.provider()
            .await
            .map_err(|_| "refused reason=identity_unproven")?;
        let settings = read_active_idle_settings(&UsbWda::new(usb.clone()))
            .await
            .map_err(reason)?;
        usb.provider()
            .await
            .map_err(|_| "refused reason=identity_unproven")?;
        Ok::<_, &'static str>(settings)
    })?;
    println!("{}", line(&settings));
    Ok(())
}

pub(super) fn restore_command(args: &[String]) -> Result<(), &'static str> {
    let usb = pinned_usb(args)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "refused")?;
    let result = runtime.block_on(async {
        usb.provider().await.map_err(|_| "refused")?;
        let result = restore_known_idle(&UsbWda::new(usb.clone())).await;
        usb.provider().await.map_err(|_| "unknown")?;
        recovery_result(result)
    })?;
    println!("restore_known_idle={result}");
    Ok(())
}

fn recovery_result(result: KnownIdleRecovery) -> Result<&'static str, &'static str> {
    match result {
        KnownIdleRecovery::Restored => Ok("restored"),
        KnownIdleRecovery::AlreadyRestored => Ok("already_restored"),
        KnownIdleRecovery::Refused => Err("refused"),
        KnownIdleRecovery::Unknown => Err("unknown"),
    }
}

fn line(settings: &IdleTimeouts) -> String {
    format!(
        "idle_settings=observed waitForIdleTimeout={} animationCoolOffTimeout={}",
        settings.wait_for_idle_timeout(),
        settings.animation_cool_off_timeout(),
    )
}

fn reason(error: SettingsError) -> &'static str {
    match error {
        SettingsError::SessionUnproven => "refused reason=session_unproven",
        _ => "refused reason=settings_unproven",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_and_error_outputs_are_fixed() {
        for args in [
            vec![],
            vec!["--config".into()],
            vec!["--other".into(), "private".into()],
        ] {
            assert_eq!(command(&args), Err("refused reason=invalid_arguments"));
            assert_eq!(
                restore_command(&args),
                Err("refused reason=invalid_arguments")
            );
        }
        for error in [
            SettingsError::ReadUnproven,
            SettingsError::InvalidSettings,
            SettingsError::ConcurrentChange,
            SettingsError::WriteUnproven,
            SettingsError::VerificationFailed,
        ] {
            assert_eq!(reason(error), "refused reason=settings_unproven");
        }
        assert_eq!(
            reason(SettingsError::SessionUnproven),
            "refused reason=session_unproven"
        );
        assert_eq!(recovery_result(KnownIdleRecovery::Restored), Ok("restored"));
        assert_eq!(
            recovery_result(KnownIdleRecovery::AlreadyRestored),
            Ok("already_restored")
        );
        assert_eq!(recovery_result(KnownIdleRecovery::Refused), Err("refused"));
        assert_eq!(recovery_result(KnownIdleRecovery::Unknown), Err("unknown"));
    }
}
