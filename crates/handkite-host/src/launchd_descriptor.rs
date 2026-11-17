//! Fixed rendering and protected staging of the per-account service descriptor.

pub(crate) mod staging;
pub use staging::{StageFailure, StagePhase, StagedDescriptor, reopen_staged, stage};
mod registration;
pub use registration::{RegistrationFailure, RegistrationOutcome, register};

use crate::service_layout::ServiceLayout;
use std::path::Path;

/// Render deterministic XML without inspecting, writing, loading, or starting anything.
///
/// The fixed staging destination is `ServiceLayout::staged_launchd_descriptor`.
/// The plist does not enforce a launchctl domain. A future installer must verify
/// the effective account's user domain, selected release, configuration, device,
/// and ownership before activation. Rendering grants none of that authority.
pub fn render(layout: &ServiceLayout) -> Result<String, &'static str> {
    let program = xml_path(&layout.current_release_executable())?;
    let stderr = xml_path(&layout.stderr_log())?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.handkite.host</string>
  <key>Program</key>
  <string>{program}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{program}</string>
    <string>--owned-wda</string>
  </array>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ExitTimeOut</key>
  <integer>180</integer>
  <key>EnableGlobbing</key>
  <false/>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>
</dict>
</plist>
"#
    ))
}

fn xml_path(path: &Path) -> Result<String, &'static str> {
    const INVALID: &str = "service path cannot be rendered as a plist string";
    let value = path.to_str().ok_or(INVALID)?;
    if !path.is_absolute()
        || value.len() > 4096
        || value[1..]
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(INVALID);
    }
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            // References preserve these characters through XML newline normalization.
            '\t' => escaped.push_str("&#9;"),
            '\n' => escaped.push_str("&#10;"),
            '\r' => escaped.push_str("&#13;"),
            '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}' => {
                escaped.push(character);
            }
            _ => return Err(INVALID),
        }
    }
    Ok(escaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_layout::tests::layout_at;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[cfg(target_os = "macos")]
    fn parse(xml: &str) -> serde_json::Value {
        use std::io::Write;
        use std::process::{Command, Stdio};
        // Apple's parser consumes only this synthetic XML through private pipes.
        let mut child = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "--", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(xml.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_plist_parser_confirms_exact_fixed_supervised_contract() {
        let layout = layout_at("/fixture/account");
        let xml = render(&layout).unwrap();
        assert_eq!(xml, render(&layout).unwrap());
        assert_eq!(
            parse(&xml),
            serde_json::json!({
                "Label": "com.handkite.host",
                "Program": "/fixture/account/Library/Application Support/Handkite/releases/current/handkited",
                "ProgramArguments": [
                    "/fixture/account/Library/Application Support/Handkite/releases/current/handkited",
                    "--owned-wda"
                ],
                "ProcessType": "Interactive",
                "RunAtLoad": true,
                "KeepAlive": true,
                "ExitTimeOut": 180,
                "EnableGlobbing": false,
                "StandardErrorPath": "/fixture/account/Library/Application Support/Handkite/runtime/handkited.stderr.log"
            })
        );
        assert_eq!(
            layout.staged_launchd_descriptor(),
            Path::new(
                "/fixture/account/Library/Application Support/Handkite/launchd/com.handkite.host.plist"
            )
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unusual_home_characters_round_trip_as_literal_arguments() {
        for home in [
            "/fixture/a & <b> \"c\" 'd'",
            "/fixture/caf\u{e9}-\u{1f98a}",
            "/fixture/tab\tline\nreturn\r",
            "/fixture/$(echo ignored); * ? [abc]",
            "/fixture/&amp;&#13;</string><key>extra</key><string>",
        ] {
            let layout = layout_at(home);
            let parsed = parse(&render(&layout).unwrap());
            assert_eq!(
                parsed["Program"],
                layout.current_release_executable().to_str().unwrap()
            );
            assert_eq!(
                parsed["ProgramArguments"],
                serde_json::json!([layout.current_release_executable(), "--owned-wda"])
            );
            assert_eq!(
                parsed["StandardErrorPath"],
                layout.stderr_log().to_str().unwrap()
            );
            assert_eq!(parsed.as_object().unwrap().len(), 9);
        }
    }

    #[test]
    fn rejects_nonrepresentable_or_ambiguous_paths_without_lossy_conversion() {
        for home in [
            "relative",
            "/fixture/../account",
            "/fixture/./account",
            "/fixture//account",
            "/fixture/null\0",
            "/fixture/control\u{1}",
            "/fixture/noncharacter\u{fffe}",
        ] {
            assert!(render(&layout_at(home)).is_err());
        }
        assert!(
            render(&layout_at(OsString::from_vec(
                b"/fixture/non-utf8-\xff".to_vec()
            )))
            .is_err()
        );
        assert!(render(&layout_at(format!("/fixture/{}", "x".repeat(4096)))).is_err());
    }
}
