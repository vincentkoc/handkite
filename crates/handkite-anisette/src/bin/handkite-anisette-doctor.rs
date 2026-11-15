use handkite_anisette::anisette_v3::{AnisetteClient, AnisetteIdentity, state};
use std::future::Future;
use std::path::Path;

const USAGE: &str = "usage: handkite-anisette-doctor --cert FILE --key FILE [--headers]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    ClientInfo,
    Headers,
}

fn options(args: &[String]) -> Result<(&Path, &Path, Mode), &'static str> {
    match args {
        [cert_flag, cert, key_flag, key] if cert_flag == "--cert" && key_flag == "--key" => {
            Ok((Path::new(cert), Path::new(key), Mode::ClientInfo))
        }
        [cert_flag, cert, key_flag, key, headers]
            if cert_flag == "--cert" && key_flag == "--key" && headers == "--headers" =>
        {
            Ok((Path::new(cert), Path::new(key), Mode::Headers))
        }
        _ => Err(USAGE),
    }
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
    let (cert, key, mode) = options(&args)?;
    let identity = AnisetteIdentity::load(cert, key)?;
    let client = AnisetteClient::new(identity)?;
    let status = check(mode, state::load_ready, |ready| async {
        match ready {
            Some(ready) => {
                client.get_headers(&ready).await?;
            }
            None => {
                client.client_info().await?;
            }
        }
        Ok(())
    })
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
        let base = ["--cert", "/private/client.pem", "--key", "/private/key.pem"];
        assert_eq!(options(&args(&base)).unwrap().2, Mode::ClientInfo);
        let mut with_headers = base.to_vec();
        with_headers.push("--headers");
        assert_eq!(options(&args(&with_headers)).unwrap().2, Mode::Headers);
        for invalid in [
            vec![],
            vec!["--headers", "--cert", "cert", "--key", "key"],
            vec!["--cert", "cert", "--key", "key", "--headers", "--headers"],
            vec!["--cert", "cert", "--key", "key", "--provision"],
        ] {
            assert!(options(&args(&invalid)).is_err());
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
