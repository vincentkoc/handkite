use handkite_anisette::anisette_v3::{AnisetteClient, AnisetteIdentity, AnisetteOrigin, state};
use std::future::Future;
use std::path::Path;

const USAGE: &str = "usage: handkite-anisette-doctor --origin https://HOST[:PORT] --cert FILE --key FILE [--headers]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    ClientInfo,
    Headers,
}

type Options<'a> = (AnisetteOrigin, &'a Path, &'a Path, Mode);

/// The origin has no default: a missing or invalid value fails before any I/O.
fn options(args: &[String]) -> Result<Options<'_>, &'static str> {
    let (origin, cert, key, mode) = match args {
        [origin_flag, origin, cert_flag, cert, key_flag, key]
            if origin_flag == "--origin" && cert_flag == "--cert" && key_flag == "--key" =>
        {
            (origin, cert, key, Mode::ClientInfo)
        }
        [origin_flag, origin, cert_flag, cert, key_flag, key, headers]
            if origin_flag == "--origin"
                && cert_flag == "--cert"
                && key_flag == "--key"
                && headers == "--headers" =>
        {
            (origin, cert, key, Mode::Headers)
        }
        _ => return Err(USAGE),
    };
    Ok((
        AnisetteOrigin::parse(origin)?,
        Path::new(cert),
        Path::new(key),
        mode,
    ))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("handkite-anisette-doctor: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), &'static str> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (origin, cert, key, mode) = options(&args)?;
    let identity = AnisetteIdentity::load(cert, key)?;
    let client = AnisetteClient::new(identity, origin)?;
    let status = check(
        mode,
        || state::load_ready(client.origin()),
        |ready| async {
            match ready {
                Some(ready) => {
                    client.get_headers(&ready).await?;
                }
                None => {
                    client.client_info().await?;
                }
            }
            Ok(())
        },
    )
    .await?;
    println!("{status}");
    Ok(())
}

async fn check<S, F, Fut>(
    mode: Mode,
    load_ready: impl FnOnce() -> Result<S, &'static str>,
    remote: F,
) -> Result<&'static str, &'static str>
where
    F: FnOnce(Option<S>) -> Fut,
    Fut: Future<Output = Result<(), &'static str>>,
{
    let ready = match mode {
        Mode::ClientInfo => None,
        Mode::Headers => Some(load_ready()?),
    };
    remote(ready).await?;
    Ok(match mode {
        Mode::ClientInfo => "anisette_client_info=valid",
        Mode::Headers => "anisette_headers=valid",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn mode_requires_exact_flags() {
        let base = [
            "--origin",
            "https://anisette.example.invalid",
            "--cert",
            "/private/client.pem",
            "--key",
            "/private/key.pem",
        ];
        let base = args(&base);
        let parsed = options(&base).unwrap();
        assert_eq!(parsed.0.to_string(), "https://anisette.example.invalid");
        assert_eq!(parsed.3, Mode::ClientInfo);
        let mut with_headers = base.clone();
        with_headers.push("--headers".into());
        assert_eq!(options(&with_headers).unwrap().3, Mode::Headers);
        for invalid in [
            vec![],
            vec!["--headers", "--cert", "cert", "--key", "key"],
            vec!["--cert", "cert", "--key", "key"],
            vec!["--cert", "cert", "--key", "key", "--headers"],
            vec![
                "--origin",
                "https://anisette.example.invalid",
                "--cert",
                "cert",
            ],
            vec![
                "--origin",
                "https://anisette.example.invalid",
                "--cert",
                "cert",
                "--key",
                "key",
                "--headers",
                "--headers",
            ],
            vec![
                "--origin",
                "https://anisette.example.invalid",
                "--cert",
                "cert",
                "--key",
                "key",
                "--provision",
            ],
        ] {
            assert_eq!(options(&args(&invalid)).err(), Some(USAGE));
        }
    }

    #[test]
    fn invalid_origin_fails_closed_before_identity_load() {
        for origin in ["", "http://anisette.example.invalid", "https://127.0.0.1"] {
            let invalid = ["--origin", origin, "--cert", "cert", "--key", "key"];
            let error = options(&args(&invalid)).err().unwrap();
            assert!(error.starts_with("Anisette origin must be https://"));
        }
    }

    #[tokio::test]
    async fn headers_require_ready_state_before_remote_request() {
        let remote = Cell::new(0);
        let result = check(
            Mode::Headers,
            || Err::<u8, _>("ready state unavailable"),
            |_| {
                remote.set(remote.get() + 1);
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result, Err("ready state unavailable"));
        assert_eq!(remote.get(), 0);
        let result = check(
            Mode::Headers,
            || Ok(7_u8),
            |ready| {
                assert_eq!(ready, Some(7));
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result, Ok("anisette_headers=valid"));
    }

    #[tokio::test]
    async fn default_mode_never_loads_ready_state() {
        let result = check(
            Mode::ClientInfo,
            || panic!("default check must not load ready state"),
            |ready: Option<u8>| {
                assert_eq!(ready, None);
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result, Ok("anisette_client_info=valid"));
    }
}
