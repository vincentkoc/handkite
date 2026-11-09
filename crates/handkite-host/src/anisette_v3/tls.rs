use rustls::{ClientConfig, RootCertStore, pki_types::PrivateKeyDer};
use rustls_pemfile::Item;
use std::ffi::CString;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::sync::Arc;
use zeroize::Zeroizing;

const MAX_CERT_BYTES: usize = 64 * 1024;
const MAX_KEY_BYTES: usize = 16 * 1024;

/// An owner-only client identity and the fixed public server trust policy.
/// This type deliberately has no Debug implementation or custom-root API.
pub struct AnisetteIdentity {
    pub(super) config: Arc<ClientConfig>,
}

impl AnisetteIdentity {
    /// Read absolute, nonsymlinked mode-0600 files in mode-0700 directories.
    pub fn load(certificate: &Path, private_key: &Path) -> Result<Self, &'static str> {
        let certificate = read_private_file(certificate, MAX_CERT_BYTES)?;
        let private_key = read_private_file(private_key, MAX_KEY_BYTES)?;
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Ok(Self {
            config: Arc::new(config_from_pem(&certificate, &private_key, roots)?),
        })
    }
}

fn read_private_file(path: &Path, limit: usize) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    const ERROR: &str = "Anisette identity file must be private and service-owned";
    if !path.is_absolute() {
        return Err(ERROR);
    }
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(ERROR);
    }
    let mut directory = File::open("/").map_err(|_| ERROR)?;
    let mut components = components.peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(ERROR);
        };
        let last = components.peek().is_none();
        if last {
            let meta = directory.metadata().map_err(|_| ERROR)?;
            if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o7777 != 0o700 {
                return Err(ERROR);
            }
        }
        let name = CString::new(name.as_bytes()).map_err(|_| ERROR)?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        // Each lookup is relative to an already-open directory, so an ancestor
        // rename cannot redirect the later ownership check or file read.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(ERROR);
        }
        let file = unsafe { File::from_raw_fd(fd) };
        if last {
            let meta = file.metadata().map_err(|_| ERROR)?;
            if !meta.is_file()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o7777 != 0o600
                || meta.nlink() != 1
                || meta.len() == 0
                || meta.len() > limit as u64
            {
                return Err(ERROR);
            }
            let mut bytes = Zeroizing::new(Vec::new());
            file.take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| ERROR)?;
            if bytes.is_empty() || bytes.len() > limit {
                return Err(ERROR);
            }
            return Ok(bytes);
        }
        directory = file;
    }
    Err(ERROR)
}

fn pem_items(bytes: &[u8]) -> Result<Vec<Item>, &'static str> {
    const ERROR: &str = "Anisette identity PEM is invalid";
    // Reject skipped text and unknown PEM labels before rustls validates the identity.
    let text = std::str::from_utf8(bytes).map_err(|_| ERROR)?;
    let mut remaining = text.trim();
    let mut items = Vec::new();
    while !remaining.is_empty() {
        let label = [
            "CERTIFICATE",
            "PRIVATE KEY",
            "RSA PRIVATE KEY",
            "EC PRIVATE KEY",
        ]
        .into_iter()
        .find(|label| remaining.starts_with(&format!("-----BEGIN {label}-----")))
        .ok_or(ERROR)?;
        let end = format!("-----END {label}-----");
        let end_offset = remaining.find(&end).ok_or(ERROR)? + end.len();
        let block = &remaining.as_bytes()[..end_offset];
        let item = rustls_pemfile::read_one(&mut &block[..])
            .map_err(|_| ERROR)?
            .ok_or(ERROR)?;
        items.push(item);
        remaining = remaining[end_offset..].trim();
    }
    Ok(items)
}

fn config_from_pem(
    certificate: &[u8],
    private_key: &[u8],
    roots: RootCertStore,
) -> Result<ClientConfig, &'static str> {
    let chain = pem_items(certificate)?
        .into_iter()
        .map(|item| match item {
            Item::X509Certificate(cert) => Ok(cert),
            _ => Err("Anisette certificate chain is invalid"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if chain.is_empty() {
        return Err("Anisette certificate chain is empty");
    }
    let mut keys = pem_items(private_key)?;
    if keys.len() != 1 {
        return Err("Anisette identity requires exactly one private key");
    }
    let key = match keys.pop().expect("one item") {
        Item::Pkcs1Key(key) => PrivateKeyDer::Pkcs1(key),
        Item::Pkcs8Key(key) => PrivateKeyDer::Pkcs8(key),
        Item::Sec1Key(key) => PrivateKeyDer::Sec1(key),
        _ => return Err("Anisette private key is invalid"),
    };
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|_| "Anisette TLS configuration failed")?
            .with_root_certificates(roots)
            .with_client_auth_cert(chain, key)
            .map_err(|_| "Anisette certificate and key are invalid or mismatched")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().canonicalize().unwrap();
        let certificate =
            rcgen::generate_simple_self_signed(vec!["fixture.invalid".into()]).unwrap();
        let cert = path.join("client.pem");
        let key = path.join("client-key.pem");
        fs::write(&cert, certificate.serialize_pem().unwrap()).unwrap();
        fs::write(&key, certificate.serialize_private_key_pem()).unwrap();
        for file in [&cert, &key] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        (dir, cert, key)
    }

    #[test]
    fn loads_private_identity_with_http1_only() {
        let (_dir, cert, key) = fixture();
        let identity = AnisetteIdentity::load(&cert, &key).unwrap();
        assert_eq!(identity.config.alpn_protocols, [b"http/1.1"]);
        assert!(identity.config.client_auth_cert_resolver.has_certs());
    }

    #[test]
    fn rejects_permissions_links_and_oversized_files() {
        let (dir, cert, key) = fixture();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(AnisetteIdentity::load(&cert, &key).is_err());
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        let link = cert.with_file_name("link.pem");
        std::os::unix::fs::symlink(&key, &link).unwrap();
        assert!(AnisetteIdentity::load(&cert, &link).is_err());
        fs::remove_file(&link).unwrap();
        fs::hard_link(&key, &link).unwrap();
        assert!(AnisetteIdentity::load(&cert, &key).is_err());
        fs::remove_file(&link).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(AnisetteIdentity::load(&cert, &key).is_err());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&key, vec![b'x'; MAX_KEY_BYTES + 1]).unwrap();
        assert!(AnisetteIdentity::load(&cert, &key).is_err());
    }

    #[test]
    fn rejects_empty_multiple_mismatched_or_unrecognized_pem() {
        let (_dir, cert, key) = fixture();
        let original = fs::read(&key).unwrap();
        for bytes in [
            Vec::new(),
            original.repeat(2),
            b"unrecognized text".to_vec(),
        ] {
            fs::write(&key, bytes).unwrap();
            assert!(AnisetteIdentity::load(&cert, &key).is_err());
        }
        let other = rcgen::generate_simple_self_signed(vec!["fixture.invalid".into()]).unwrap();
        fs::write(&key, other.serialize_private_key_pem()).unwrap();
        assert!(AnisetteIdentity::load(&cert, &key).is_err());
        fs::write(&key, original).unwrap();
        fs::write(&cert, b" \n").unwrap();
        assert!(AnisetteIdentity::load(&cert, &key).is_err());
    }
}
