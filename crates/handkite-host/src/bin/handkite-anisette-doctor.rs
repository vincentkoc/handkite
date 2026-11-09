use handkite_host::anisette_v3::{AnisetteClient, AnisetteIdentity};
use std::path::Path;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("handkite-anisette-doctor: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), &'static str> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 || args[1] != "--cert" || args[3] != "--key" {
        return Err("usage: handkite-anisette-doctor --cert FILE --key FILE");
    }
    let identity = AnisetteIdentity::load(Path::new(&args[2]), Path::new(&args[4]))?;
    let client = AnisetteClient::new(identity)?;
    client.client_info().await?;
    println!("anisette_client_info=valid");
    Ok(())
}
