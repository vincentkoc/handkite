pub(crate) fn validate_runner_bundle_id(bundle_id: &str) -> Result<(), &'static str> {
    let runner = if let Some(base) = bundle_id.strip_suffix(".xctrunner") {
        base
    } else if let Some((base, team)) = bundle_id.rsplit_once(".xctrunner.") {
        if team.len() != 10
            || !team
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            return Err("WDA runner bundle ID is invalid");
        }
        base
    } else {
        return Err("WDA runner bundle ID is invalid");
    };
    if bundle_id.len() > 255
        || runner.split('.').count() < 2
        || !runner.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err("WDA runner bundle ID is invalid");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_exact_runner_and_signed_team_suffix() {
        for value in [
            "com.example.xctrunner",
            "com.example.WDA-Runner.xctrunner",
            "com.example.WDA-Runner.xctrunner.ABCDE12345",
        ] {
            assert!(validate_runner_bundle_id(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn refuses_ambiguous_or_malformed_runner_suffix() {
        for value in [
            "com.example.WDA-Runner.xctrunner.ABCDE1234",
            "com.example.WDA-Runner.xctrunner.abcde12345",
            "com.example.WDA-Runner.xctrunner.ABCDE12345.extra",
            "com.example.WDA-Runner.xctrunner.",
            "com.example..xctrunner.ABCDE12345",
            "com.example.WDA-Runner.xctrunner.ABCDE1234_",
        ] {
            assert!(validate_runner_bundle_id(value).is_err(), "{value}");
        }
    }
}
