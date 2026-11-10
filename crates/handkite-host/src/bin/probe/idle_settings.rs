use handkite_host::host_config;
use handkite_host::usb::PinnedUsb;
use handkite_host::wda_settings::{IdleTimeouts, SettingsError, read_active_idle_settings};
use handkite_host::wda_transport::UsbWda;
use std::path::Path;

pub(super) fn command(args: &[String]) -> Result<(), &'static str> {
    let [flag, path] = args else {
        return Err("refused reason=invalid_arguments");
    };
    if flag != "--config" {
        return Err("refused reason=invalid_arguments");
    }
    let binding =
        host_config::load(Path::new(path)).map_err(|_| "refused reason=configuration_invalid")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "refused reason=settings_unproven")?;
    let usb = PinnedUsb::new(binding.usb_id);
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
    }
}
