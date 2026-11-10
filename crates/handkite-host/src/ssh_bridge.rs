//! Pure rendering of a restricted OpenSSH authorized_keys entry.

use crate::mcp::Capabilities;
use crate::service_layout::ServiceLayout;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::path::Path;

/// Exact byte length of a bare, canonical ssh-ed25519 public key, without LF.
pub const PUBLIC_KEY_BYTES: usize = 80;

/// Render one LF-terminated entry without reading or changing service or SSH state.
///
/// The caller must choose capabilities explicitly. The command never references
/// SSH_ORIGINAL_COMMAND or accepts caller-selected endpoints. This validates key
/// wire syntax, not ownership, authentication, installation, or account isolation.
pub fn render(
    layout: &ServiceLayout,
    public_key: &str,
    capabilities: Capabilities,
) -> Result<String, &'static str> {
    validate_key(public_key)?;
    let executable = layout.current_release_mcp_executable();
    let path = command_path(&executable)?;
    let capabilities = match capabilities {
        Capabilities::Observation => "observation",
        Capabilities::Navigation => "navigation",
        Capabilities::ObservationAndNavigation => "observation,navigation",
    };
    Ok(format!(
        "restrict,command=\"exec '{path}' --service --capabilities {capabilities}\" {public_key}\n"
    ))
}

fn validate_key(key: &str) -> Result<(), &'static str> {
    const INVALID: &str = "invalid SSH public key";
    if key.len() != PUBLIC_KEY_BYTES {
        return Err(INVALID);
    }
    let encoded = key.strip_prefix("ssh-ed25519 ").ok_or(INVALID)?;
    let decoded = STANDARD.decode(encoded).map_err(|_| INVALID)?;
    // SSH strings: algorithm length + algorithm, then public-key length + 32 bytes.
    if decoded.len() != 51
        || decoded[..19] != *b"\0\0\0\x0bssh-ed25519\0\0\0\x20"
        || STANDARD.encode(&decoded) != encoded
    {
        return Err(INVALID);
    }
    Ok(())
}

fn command_path(path: &Path) -> Result<&str, &'static str> {
    const INVALID: &str = "unsafe SSH bridge path";
    let value = path.to_str().ok_or(INVALID)?;
    if !path.is_absolute()
        || value.len() > 4096
        || value[1..]
            .split('/')
            .any(|part| matches!(part, "" | "." | ".."))
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b' ' | b'.' | b'_' | b'-')
        })
    {
        return Err(INVALID);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_layout::tests::layout_at;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn wire_key() -> Vec<u8> {
        let mut bytes = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
        bytes.extend_from_slice(&[0x42; 32]);
        bytes
    }

    fn key() -> String {
        format!("ssh-ed25519 {}", STANDARD.encode(wire_key()))
    }

    fn capabilities() -> [(Capabilities, &'static str); 3] {
        [
            (Capabilities::Observation, "observation"),
            (Capabilities::Navigation, "navigation"),
            (
                Capabilities::ObservationAndNavigation,
                "observation,navigation",
            ),
        ]
    }

    #[test]
    fn exact_entry_fixes_executable_endpoint_and_each_explicit_capability() {
        let layout = layout_at("/fixture/account");
        let key = key();
        assert_eq!(key.len(), PUBLIC_KEY_BYTES);
        for (capability, value) in capabilities() {
            let rendered = render(&layout, &key, capability).unwrap();
            assert_eq!(
                rendered,
                format!(
                    "restrict,command=\"exec '/fixture/account/Library/Application Support/Handkite/releases/current/handkite-mcp' --service --capabilities {value}\" {key}\n"
                )
            );
            assert_eq!(rendered.lines().count(), 1);
        }
    }

    #[test]
    fn key_options_comments_whitespace_and_noncanonical_text_are_rejected() {
        let key = key();
        for invalid in [
            String::new(),
            format!(" {key}"),
            format!("{key} "),
            format!("{key}\n"),
            format!("{key}\r\n"),
            format!("{key}\n{key}"),
            format!("{key} comment"),
            format!("restrict {key}"),
            format!("command=\"false\" {key}"),
            key.replacen(' ', "\t", 1),
            key.replacen(' ', "  ", 1),
            key.replace("ssh-ed25519", "ssh-rsa"),
            key.replace("ssh-ed25519", "ssh-ed25519-cert-v01@openssh.com"),
            format!("{key}="),
            key[..key.len() - 1].to_owned(),
            format!("{}!", &key[..key.len() - 1]),
            "x".repeat(8192),
        ] {
            assert_eq!(validate_key(&invalid), Err("invalid SSH public key"));
        }
    }

    #[test]
    fn malformed_wire_algorithm_lengths_and_trailing_data_are_rejected() {
        let mut invalid = Vec::new();
        for index in [0, 3, 4, 14, 15, 18] {
            let mut bytes = wire_key();
            bytes[index] ^= 1;
            invalid.push(bytes);
        }
        let mut truncated = wire_key();
        truncated.pop();
        invalid.push(truncated);
        let mut trailing = wire_key();
        trailing.push(0);
        invalid.push(trailing);
        for bytes in invalid {
            let key = format!("ssh-ed25519 {}", STANDARD.encode(bytes));
            assert_eq!(validate_key(&key), Err("invalid SSH public key"));
        }
    }

    #[test]
    fn unsafe_or_noncanonical_account_paths_are_rejected() {
        for home in [
            "relative",
            "//fixture/account",
            "/fixture//account",
            "/fixture/./account",
            "/fixture/../account",
            "/fixture/a'b",
            "/fixture/a\"b",
            "/fixture/a\\b",
            "/fixture/a$b",
            "/fixture/`command`",
            "/fixture/a;b",
            "/fixture/a,b",
            "/fixture/a\nb",
            "/fixture/a\rb",
            "/fixture/a\tb",
            "/fixture/a\0b",
            "/fixture/\u{7f}",
            "/fixture/\u{e9}",
        ] {
            assert_eq!(
                render(&layout_at(home), &key(), Capabilities::Observation),
                Err("unsafe SSH bridge path")
            );
        }
        let non_utf8 = OsString::from_vec(b"/fixture/\xff".to_vec());
        assert!(render(&layout_at(non_utf8), &key(), Capabilities::Observation).is_err());
    }

    #[test]
    fn path_length_is_bounded_below_the_authorized_keys_line_limit() {
        let suffix = "/Library/Application Support/Handkite/releases/current/handkite-mcp";
        let home = format!("/{}", "a".repeat(4096 - suffix.len() - 1));
        let line = render(&layout_at(&home), &key(), Capabilities::Observation).unwrap();
        assert!(line.len() < 8192);
        assert!(
            render(
                &layout_at(format!("{home}a")),
                &key(),
                Capabilities::Observation
            )
            .is_err()
        );
    }

    #[test]
    fn shell_executes_exact_argv_and_ignores_hostile_original_commands() {
        let scratch = tempfile::tempdir().unwrap();
        let layout = layout_at(scratch.path().join("account with spaces"));
        let executable = layout.current_release_mcp_executable();
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"#!/bin/sh\nprintf '%s\\0' \"$0\" \"$@\"\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = scratch.path().join("unexpected");
        for (capability, value) in capabilities() {
            let line = render(&layout, &key(), capability).unwrap();
            let command = line
                .strip_prefix("restrict,command=\"")
                .unwrap()
                .split_once("\" ")
                .unwrap()
                .0;
            for original in [
                "--socket /fixture/other.sock --device other --capabilities all".to_owned(),
                format!("'; /usr/bin/touch '{}'; #", marker.display()),
                format!("$(/usr/bin/touch '{}')\nexec /bin/sh", marker.display()),
                "exec /bin/sh -c 'printf compromised'".to_owned(),
            ] {
                let output = Command::new("/bin/sh")
                    .args(["-c", command])
                    .env_clear()
                    .env("SSH_ORIGINAL_COMMAND", original)
                    .env("HOME", "/fixture/ignored")
                    .env("PATH", "/fixture/ignored")
                    .output()
                    .unwrap();
                assert!(output.status.success());
                assert!(output.stderr.is_empty());
                assert_eq!(
                    output.stdout,
                    format!(
                        "{}\0--service\0--capabilities\0{value}\0",
                        executable.display()
                    )
                    .as_bytes()
                );
                assert!(!marker.exists());
            }
        }
    }
}
