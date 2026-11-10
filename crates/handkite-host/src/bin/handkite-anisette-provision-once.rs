use handkite_host::anisette_v3::{AnisetteClient, AnisetteIdentity, state::PendingProvisioning};
use std::future::Future;
use std::path::Path;

const USAGE: &str = "usage: handkite-anisette-provision-once --cert FILE --key FILE";

fn options(args: &[String]) -> Result<(&Path, &Path), &'static str> {
    match args {
        [cert_flag, cert, key_flag, key] if cert_flag == "--cert" && key_flag == "--key" => {
            Ok((Path::new(cert), Path::new(key)))
        }
        _ => Err(USAGE),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = execute(
        &args,
        |cert, key| {
            let identity = AnisetteIdentity::load(cert, key).map_err(|_| ())?;
            AnisetteClient::new(identity).map_err(|_| ())
        },
        || PendingProvisioning::reserve().map_err(|_| ()),
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
    load_identity: impl FnOnce(&Path, &Path) -> Result<I, ()>,
    reserve: impl FnOnce() -> Result<P, ()>,
    provision: F,
) -> Result<&'static str, &'static str>
where
    F: FnOnce(I, P) -> Fut,
    Fut: Future<Output = Result<(), ()>>,
{
    let (cert, key) = options(args)?;
    let identity = load_identity(cert, key).map_err(|_| "identity_unavailable")?;
    let pending = reserve().map_err(|_| "reservation_unavailable")?;
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

    #[test]
    fn requires_exact_identity_arguments() {
        assert!(options(&args(&["--cert", "/private/cert", "--key", "/private/key"])).is_ok());
        for invalid in [
            vec![],
            vec!["--key", "key", "--cert", "cert"],
            vec!["--cert", "cert", "--key", "key", "--retry"],
            vec!["--cert", "cert", "--key", "key", "--state", "elsewhere"],
        ] {
            assert!(options(&args(&invalid)).is_err());
        }
    }

    #[tokio::test]
    async fn identity_failure_does_not_reserve_or_contact_server() {
        let reserve_count = Cell::new(0);
        let provision_count = Cell::new(0);
        let result = execute(
            &args(&["--cert", "cert", "--key", "key"]),
            |_, _| Err::<u8, _>(()),
            || {
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
            &args(&["--cert", "cert", "--key", "key"]),
            |_, _| Ok(1_u8),
            || Err::<u8, _>(()),
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
            &args(&["--cert", "cert", "--key", "key"]),
            |_, _| {
                assert_eq!(stage.replace(1), 0);
                Ok(1_u8)
            },
            || {
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
            &args(&["--cert", "cert", "--key", "key"]),
            |_, _| Ok(1_u8),
            || Ok(2_u8),
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
