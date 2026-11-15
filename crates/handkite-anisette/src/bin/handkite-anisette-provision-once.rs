use handkite_anisette::anisette_v3::{
    AnisetteClient, AnisetteIdentity, AnisetteOrigin, state::PendingProvisioning,
};
use std::future::Future;
use std::path::Path;

const USAGE: &str =
    "usage: handkite-anisette-provision-once --origin https://HOST[:PORT] --cert FILE --key FILE";

/// The origin has no default: a missing or invalid value fails before any I/O.
fn options(args: &[String]) -> Result<(AnisetteOrigin, &Path, &Path), &'static str> {
    match args {
        [origin_flag, origin, cert_flag, cert, key_flag, key]
            if origin_flag == "--origin" && cert_flag == "--cert" && key_flag == "--key" =>
        {
            Ok((
                AnisetteOrigin::parse(origin)?,
                Path::new(cert),
                Path::new(key),
            ))
        }
        _ => Err(USAGE),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = execute(
        &args,
        |origin, cert, key| {
            let identity = AnisetteIdentity::load(cert, key).map_err(|_| ())?;
            AnisetteClient::new(identity, origin).map_err(|_| ())
        },
        |client: &AnisetteClient| PendingProvisioning::reserve(client.origin()).map_err(|_| ()),
        |client, pending| async move {
            client
                .provision_apple(pending)
                .await
                .map(|_| ())
                .map_err(|_| ())
        },
    )
    .await;
    match result {
        Ok(status) => println!("{status}"),
        Err(error) => {
            eprintln!("handkite-anisette-provision-once: {error}");
            std::process::exit(1);
        }
    }
}

async fn execute<I, P, F, Fut>(
    args: &[String],
    load_identity: impl FnOnce(AnisetteOrigin, &Path, &Path) -> Result<I, ()>,
    reserve: impl FnOnce(&I) -> Result<P, ()>,
    provision: F,
) -> Result<&'static str, &'static str>
where
    F: FnOnce(I, P) -> Fut,
    Fut: Future<Output = Result<(), ()>>,
{
    let (origin, cert, key) = options(args)?;
    let identity = load_identity(origin, cert, key).map_err(|_| "identity_unavailable")?;
    let pending = reserve(&identity).map_err(|_| "reservation_unavailable")?;
    provision(identity, pending)
        .await
        .map_err(|_| "provisioning_failed")?;
    Ok("anisette_provisioning=ready")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    const ORIGIN: [&str; 2] = ["--origin", "https://anisette.example.invalid"];

    fn full(rest: &[&'static str]) -> Vec<String> {
        args(&[&ORIGIN[..], rest].concat())
    }

    #[test]
    fn requires_exact_origin_and_identity_arguments() {
        let valid = full(&["--cert", "/private/cert", "--key", "/private/key"]);
        let parsed = options(&valid).unwrap();
        assert_eq!(parsed.0.to_string(), "https://anisette.example.invalid");
        for invalid in [
            vec![],
            vec!["--cert", "cert", "--key", "key"],
            vec!["--key", "key", "--cert", "cert"],
            vec!["--cert", "cert", "--key", "key", "--origin", ORIGIN[1]],
        ] {
            assert_eq!(options(&args(&invalid)).err(), Some(USAGE));
        }
        for invalid in [
            vec!["--key", "key", "--cert", "cert"],
            vec!["--cert", "cert", "--key", "key", "--retry"],
            vec!["--cert", "cert", "--key", "key", "--state", "elsewhere"],
        ] {
            assert_eq!(options(&full(&invalid)).err(), Some(USAGE));
        }
        let invalid_origin = ["--origin", "https://localhost", "--cert", "c", "--key", "k"];
        assert!(
            options(&args(&invalid_origin))
                .err()
                .unwrap()
                .starts_with("Anisette origin must be https://")
        );
    }

    #[tokio::test]
    async fn identity_failure_does_not_reserve_or_contact_server() {
        let reserve_count = Cell::new(0);
        let provision_count = Cell::new(0);
        let result = execute(
            &full(&["--cert", "cert", "--key", "key"]),
            |_, _, _| Err::<u8, _>(()),
            |_| {
                reserve_count.set(reserve_count.get() + 1);
                Ok(1_u8)
            },
            |_, _| {
                provision_count.set(provision_count.get() + 1);
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result, Err("identity_unavailable"));
        assert_eq!(reserve_count.get(), 0);
        assert_eq!(provision_count.get(), 0);
    }

    #[tokio::test]
    async fn reservation_failure_does_not_contact_server() {
        let provision_count = Cell::new(0);
        let result = execute(
            &full(&["--cert", "cert", "--key", "key"]),
            |_, _, _| Ok(1_u8),
            |_| Err::<u8, _>(()),
            |_, _| {
                provision_count.set(provision_count.get() + 1);
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result, Err("reservation_unavailable"));
        assert_eq!(provision_count.get(), 0);
    }

    #[tokio::test]
    async fn one_reservation_is_consumed_by_one_provisioning_call() {
        let stage = Cell::new(0);
        let result = execute(
            &full(&["--cert", "cert", "--key", "key"]),
            |origin, _, _| {
                assert_eq!(origin.to_string(), ORIGIN[1]);
                assert_eq!(stage.replace(1), 0);
                Ok(1_u8)
            },
            |identity| {
                assert_eq!(*identity, 1);
                assert_eq!(stage.replace(2), 1);
                Ok(2_u8)
            },
            |identity, pending| {
                assert_eq!((identity, pending), (1, 2));
                assert_eq!(stage.replace(3), 2);
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result, Ok("anisette_provisioning=ready"));
        assert_eq!(stage.get(), 3);
    }

    #[tokio::test]
    async fn provisioning_failure_has_no_second_call() {
        let calls = Cell::new(0);
        let result = execute(
            &full(&["--cert", "cert", "--key", "key"]),
            |_, _, _| Ok(1_u8),
            |_| Ok(2_u8),
            |_, _| {
                calls.set(calls.get() + 1);
                async { Err(()) }
            },
        )
        .await;
        assert_eq!(result, Err("provisioning_failed"));
        assert_eq!(calls.get(), 1);
    }
}
