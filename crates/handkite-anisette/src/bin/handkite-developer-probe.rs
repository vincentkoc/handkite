//! Explicit read-only compatibility probe. No account login, signing, or device mutation.

use handkite_anisette::anisette_v3::{AnisetteClient, AnisetteIdentity, AnisetteOrigin};
use handkite_anisette::developer_probe::{DeveloperCredential, list_teams};
use std::fs::File;
use std::future::Future;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResultKind {
    InvalidArguments,
    IdentityUnavailable,
    CredentialUnavailable,
    RequestFailed,
    Valid,
}

impl ResultKind {
    fn report(self) -> (u8, &'static str) {
        match self {
            Self::InvalidArguments => (2, "developer_probe=invalid_arguments\n"),
            Self::IdentityUnavailable => (1, "developer_probe=identity_unavailable\n"),
            Self::CredentialUnavailable => (1, "developer_probe=credential_unavailable\n"),
            Self::RequestFailed => (1, "developer_probe=request_failed\n"),
            Self::Valid => (0, "developer_probe=teams_valid\n"),
        }
    }
}

/// `--origin` has no default; a missing or invalid origin is invalid arguments.
fn options(args: &[String]) -> Option<(AnisetteOrigin, &Path, &Path)> {
    let [origin_flag, origin, cert_flag, cert, key_flag, key] = args else {
        return None;
    };
    if origin_flag != "--origin" || cert_flag != "--cert" || key_flag != "--key" {
        return None;
    }
    let origin = AnisetteOrigin::parse(origin).ok()?;
    Some((origin, Path::new(cert), Path::new(key)))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match io::stdin().as_fd().try_clone_to_owned() {
        Ok(fd) => {
            run(
                &args,
                File::from(fd),
                |origin, cert, key| {
                    let identity = AnisetteIdentity::load(cert, key).map_err(|_| ())?;
                    AnisetteClient::new(identity, origin).map_err(|_| ())
                },
                DeveloperCredential::read_once,
                |client, credential| async move {
                    list_teams(&client, credential).await.map_err(|_| ())
                },
            )
            .await
        }
        Err(_) => ResultKind::CredentialUnavailable,
    };
    let (code, output) = result.report();
    if io::stdout().lock().write_all(output.as_bytes()).is_err() {
        return ExitCode::from(1);
    }
    ExitCode::from(code)
}

async fn run<T, U, I, C, F, Fut>(
    args: &[String],
    input: File,
    identity: I,
    credential: C,
    request: F,
) -> ResultKind
where
    I: FnOnce(AnisetteOrigin, &Path, &Path) -> Result<T, ()>,
    C: FnOnce(File) -> Result<U, &'static str>,
    F: FnOnce(T, U) -> Fut,
    Fut: Future<Output = Result<(), ()>>,
{
    let Some((origin, cert, key)) = options(args) else {
        return ResultKind::InvalidArguments;
    };
    let Ok(client) = identity(origin, cert, key) else {
        return ResultKind::IdentityUnavailable;
    };
    let Ok(credential) = credential(input) else {
        return ResultKind::CredentialUnavailable;
    };
    match request(client, credential).await {
        Ok(()) => ResultKind::Valid,
        Err(()) => ResultKind::RequestFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARGS: [&str; 6] = [
        "--origin",
        "https://anisette.example.invalid",
        "--cert",
        "/private/cert",
        "--key",
        "/private/key",
    ];

    #[test]
    fn accepts_only_exact_arguments() {
        let args = ARGS.map(str::to_owned);
        assert!(options(&args).is_some());
        for values in [
            vec![],
            vec!["--cert", "cert", "--key", "key"],
            vec!["--key", "key", "--cert", "cert"],
            vec![
                "--origin",
                "https://anisette.example.invalid",
                "--cert",
                "c",
            ],
            vec![
                "--origin",
                "http://anisette.example.invalid",
                "--cert",
                "c",
                "--key",
                "k",
            ],
            vec!["--origin", "", "--cert", "c", "--key", "k"],
            vec!["--cert", "cert", "--key", "key", "--retry"],
            vec!["--cert", "cert", "--key", "key", "--token", "secret"],
        ] {
            assert!(options(&values.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_none());
        }
        assert_eq!(
            ResultKind::Valid.report(),
            (0, "developer_probe=teams_valid\n")
        );
        assert_eq!(
            ResultKind::RequestFailed.report().1,
            "developer_probe=request_failed\n"
        );
    }

    #[tokio::test]
    async fn invalid_identity_and_credential_never_send_request() {
        let args = ARGS.map(str::to_owned);
        let file = File::open("/dev/null").unwrap();
        let result = run(
            &args,
            file,
            |_, _, _| Err::<u8, _>(()),
            |_| -> Result<u8, &'static str> { panic!("credential read") },
            |_, _| async { panic!("network") },
        )
        .await;
        assert_eq!(result, ResultKind::IdentityUnavailable);
        let file = File::open("/dev/null").unwrap();
        let result = run(
            &args,
            file,
            |_, _, _| Ok(1_u8),
            |_| Err::<u8, _>("invalid"),
            |_, _| async { panic!("network") },
        )
        .await;
        assert_eq!(result, ResultKind::CredentialUnavailable);
    }

    #[tokio::test]
    async fn accepted_input_sends_exactly_one_request() {
        use std::cell::Cell;
        let args = ARGS.map(str::to_owned);
        let calls = Cell::new(0);
        let result = run(
            &args,
            File::open("/dev/null").unwrap(),
            |_, _, _| Ok(1_u8),
            |_| Ok::<_, &'static str>(2_u8),
            |client, credential| {
                assert_eq!((client, credential), (1, 2));
                calls.set(calls.get() + 1);
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result, ResultKind::Valid);
        assert_eq!(calls.get(), 1);
    }
}
