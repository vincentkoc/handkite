pub(crate) fn validate_runner_bundle_id(bundle_id: &str) -> Result<(), &'static str> {
    if bundle_id.len() > 255
        || !bundle_id.ends_with(".xctrunner")
        || bundle_id.split('.').count() < 3
        || !bundle_id.split('.').all(|part| {
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
