use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;

const DEV_CERT_FILE: &str = "self-signed-cert.pem";
const DEV_KEY_FILE: &str = "self-signed-key.pem";

fn dev_cert_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    base.map(|b| b.join("ScreenExtend"))
}

pub struct TlsMaterial {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub self_signed: bool,
}

/// Hand-written rather than derived: `key_pem` is a private key, and a derived `Debug` would put
/// it in the log the first time anyone wrote `{material:?}` — including through an
/// `expect`/`unwrap` message, which is how it would happen.
impl std::fmt::Debug for TlsMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsMaterial")
            .field("cert_pem_bytes", &self.cert_pem.len())
            .field("key_pem", &"<redacted>")
            .field("self_signed", &self.self_signed)
            .finish()
    }
}

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn load_or_generate(
    cert_path: Option<&str>,
    key_path: Option<&str>,
    extra_sans: &[String],
) -> Result<TlsMaterial> {
    match (cert_path, key_path) {
        (Some(c), Some(k)) => {
            let cert_pem =
                std::fs::read(c).with_context(|| format!("reading TLS cert from {c}"))?;
            let key_pem = std::fs::read(k).with_context(|| format!("reading TLS key from {k}"))?;
            tprintln!("TLS: using operator-provided certificate (cert={c}, key={k})");
            Ok(TlsMaterial {
                cert_pem,
                key_pem,
                self_signed: false,
            })
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!(
                "TLS needs BOTH --tls-cert and --tls-key (or neither, for a dev self-signed cert)"
            )
        }
        (None, None) => generate_or_load_dev_cert(extra_sans),
    }
}

pub async fn rustls_config(material: &TlsMaterial) -> Result<RustlsConfig> {
    install_crypto_provider();
    RustlsConfig::from_pem(material.cert_pem.clone(), material.key_pem.clone())
        .await
        .context("building rustls config from PEM cert/key")
}

fn generate_or_load_dev_cert(extra_sans: &[String]) -> Result<TlsMaterial> {
    let dir = dev_cert_dir();
    let (cert_file, key_file) = match &dir {
        Some(d) => (d.join(DEV_CERT_FILE), d.join(DEV_KEY_FILE)),
        None => (PathBuf::from(DEV_CERT_FILE), PathBuf::from(DEV_KEY_FILE)),
    };

    if dir.is_some() {
        migrate_stray_cwd_cert();
    }

    if let Some(cached) = read_cached_cert(&cert_file, &key_file) {
        tprintln!(
            "TLS: reusing cached dev self-signed certificate ({})",
            cert_file.display()
        );
        return Ok(cached);
    }

    let sans = subject_alt_names(extra_sans);
    tprintln!("TLS: generating dev self-signed certificate (SANs: {sans:?})");
    let material = generate_self_signed(&sans)?;

    if let Err(e) = cache_dev_cert(&cert_file, &material.cert_pem, &key_file, &material.key_pem) {
        teprintln!("TLS: could not cache dev cert ({e}); it will be regenerated next launch");
    }

    Ok(material)
}

/// The cached pair, or `None` when either half is missing or unreadable.
///
/// Both halves or neither: a certificate without its key is not usable, and a half-written
/// cache (a crash mid-`cache_dev_cert`, a key the current user cannot read) has to fall through
/// to a regeneration rather than fail the server start.
fn read_cached_cert(cert_file: &Path, key_file: &Path) -> Option<TlsMaterial> {
    if !cert_file.exists() || !key_file.exists() {
        return None;
    }
    match (std::fs::read(cert_file), std::fs::read(key_file)) {
        (Ok(cert_pem), Ok(key_pem)) => Some(TlsMaterial {
            cert_pem,
            key_pem,
            self_signed: true,
        }),
        _ => None,
    }
}

/// A fresh self-signed certificate over `sans`, in PEM.
///
/// A new key pair every call, which is why the cache exists: a client that has accepted this
/// host's certificate once should not be asked again on the next launch.
fn generate_self_signed(sans: &[String]) -> Result<TlsMaterial> {
    let mut params = rcgen::CertificateParams::new(sans.to_vec())
        .context("building self-signed certificate params")?;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "ScreenExtend");
    params.distinguished_name = dn;

    let key_pair = rcgen::KeyPair::generate().context("generating certificate key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("generating self-signed certificate")?;
    Ok(TlsMaterial {
        cert_pem: cert.pem().into_bytes(),
        key_pem: key_pair.serialize_pem().into_bytes(),
        self_signed: true,
    })
}

fn cache_dev_cert(
    cert_file: &Path,
    cert_pem: &[u8],
    key_file: &Path,
    key_pem: &[u8],
) -> Result<()> {
    if let Some(parent) = key_file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating cert dir {}", parent.display()))?;
        }
    }
    std::fs::write(cert_file, cert_pem)
        .with_context(|| format!("writing {}", cert_file.display()))?;
    write_private_file(key_file, key_pem)
        .with_context(|| format!("writing {}", key_file.display()))?;
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
    }
    Ok(())
}

fn migrate_stray_cwd_cert() {
    for name in [DEV_CERT_FILE, DEV_KEY_FILE] {
        let stray = PathBuf::from(name);
        if stray.exists() {
            match std::fs::remove_file(&stray) {
                Ok(()) => tprintln!(
                    "TLS: removed stray dev cert file '{name}' from the working directory; \
                     it now lives in the app data dir"
                ),
                Err(e) => teprintln!("TLS: could not remove stray '{name}' from working dir: {e}"),
            }
        }
    }
}

fn subject_alt_names(extra_sans: &[String]) -> Vec<String> {
    let mut sans = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    for s in extra_sans {
        if !s.is_empty() && !sans.contains(s) {
            sans.push(s.clone());
        }
    }
    sans
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that removes itself, so the cache round trip runs against real files
    /// without touching the app data dir the running host uses.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "screenextend-tls-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            TempDir(dir)
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn localhost_and_the_loopback_address_are_always_offered() {
        // The client reaches the host by IP on the LAN and by `localhost` on the same machine.
        // Losing either name turns every join into a certificate warning.
        let sans = subject_alt_names(&[]);
        assert!(sans.contains(&"localhost".to_string()));
        assert!(sans.contains(&"127.0.0.1".to_string()));
    }

    #[test]
    fn extra_sans_are_appended_once_and_blanks_are_dropped() {
        // The extras are this host's adapter addresses, enumerated per interface, so a machine
        // on two networks routinely offers the same address twice; an empty string comes from an
        // adapter with no address at all and is not a name anything can match.
        let sans = subject_alt_names(&[
            "192.168.1.10".into(),
            "192.168.1.10".into(),
            String::new(),
            "127.0.0.1".into(),
            "10.0.0.4".into(),
        ]);
        assert_eq!(
            sans,
            vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "192.168.1.10".to_string(),
                "10.0.0.4".to_string(),
            ]
        );
    }

    #[test]
    fn a_generated_certificate_is_pem_and_carries_a_fresh_key_each_time() {
        let sans = subject_alt_names(&["192.168.1.10".into()]);
        let a = generate_self_signed(&sans).expect("generate");
        let b = generate_self_signed(&sans).expect("generate");
        let cert = String::from_utf8(a.cert_pem.clone()).expect("cert is utf-8 PEM");
        let key = String::from_utf8(a.key_pem.clone()).expect("key is utf-8 PEM");
        assert!(
            cert.starts_with("-----BEGIN CERTIFICATE-----"),
            "{cert:.40}"
        );
        assert!(key.contains("PRIVATE KEY-----"), "{key:.40}");
        assert!(a.self_signed);
        // This is exactly why the cache exists: without it a client that trusted this host once
        // is asked again on every launch.
        assert_ne!(a.key_pem, b.key_pem, "each generation is a new key pair");
    }

    #[tokio::test]
    async fn rustls_accepts_what_we_generate() {
        // The one assertion that matters: the certificate this code produces is one the server
        // will actually serve. A malformed PEM here fails at bind time on the user's machine
        // with no test between here and there.
        let material = generate_self_signed(&subject_alt_names(&[])).expect("generate");
        rustls_config(&material)
            .await
            .expect("rustls should accept a freshly generated self-signed pair");
    }

    #[test]
    fn the_cache_round_trips_byte_for_byte() {
        let dir = TempDir::new("roundtrip");
        let cert_file = dir.join(DEV_CERT_FILE);
        let key_file = dir.join(DEV_KEY_FILE);
        assert!(
            read_cached_cert(&cert_file, &key_file).is_none(),
            "empty dir"
        );

        let material = generate_self_signed(&subject_alt_names(&[])).expect("generate");
        cache_dev_cert(&cert_file, &material.cert_pem, &key_file, &material.key_pem)
            .expect("cache");

        let cached = read_cached_cert(&cert_file, &key_file).expect("cached pair");
        assert_eq!(cached.cert_pem, material.cert_pem);
        assert_eq!(cached.key_pem, material.key_pem);
        assert!(cached.self_signed);
    }

    #[test]
    fn half_a_cache_is_not_a_cache() {
        // A crash between the two writes, or a key the current user cannot read, must fall
        // through to a regeneration rather than handing rustls a certificate with no key.
        let dir = TempDir::new("half");
        let cert_file = dir.join(DEV_CERT_FILE);
        let key_file = dir.join(DEV_KEY_FILE);
        let material = generate_self_signed(&subject_alt_names(&[])).expect("generate");

        std::fs::write(&cert_file, &material.cert_pem).expect("write cert");
        assert!(
            read_cached_cert(&cert_file, &key_file).is_none(),
            "a certificate with no key is unusable"
        );

        std::fs::remove_file(&cert_file).expect("rm cert");
        std::fs::write(&key_file, &material.key_pem).expect("write key");
        assert!(
            read_cached_cert(&cert_file, &key_file).is_none(),
            "a key with no certificate is unusable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_cached_private_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("perms");
        let cert_file = dir.join(DEV_CERT_FILE);
        let key_file = dir.join(DEV_KEY_FILE);
        let material = generate_self_signed(&subject_alt_names(&[])).expect("generate");
        cache_dev_cert(&cert_file, &material.cert_pem, &key_file, &material.key_pem)
            .expect("cache");
        let mode = std::fs::metadata(&key_file)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "key mode is {:o}", mode & 0o777);
    }

    #[test]
    fn an_operator_certificate_needs_both_halves() {
        // Half a pair is a configuration mistake, not a reason to fall back to a self-signed
        // certificate the operator did not ask for — that would look like it worked.
        let err = load_or_generate(Some("cert.pem"), None, &[]).unwrap_err();
        assert!(err.to_string().contains("BOTH"), "{err}");
        let err = load_or_generate(None, Some("key.pem"), &[]).unwrap_err();
        assert!(err.to_string().contains("BOTH"), "{err}");
    }

    #[test]
    fn an_operator_certificate_is_read_verbatim_and_is_not_self_signed() {
        // `self_signed` drives the HTTPS upsell modal on the client page; reporting a real
        // certificate as self-signed would show a warning nobody needs to see.
        let dir = TempDir::new("operator");
        let cert_file = dir.join("operator-cert.pem");
        let key_file = dir.join("operator-key.pem");
        let material = generate_self_signed(&subject_alt_names(&[])).expect("generate");
        std::fs::write(&cert_file, &material.cert_pem).expect("write cert");
        std::fs::write(&key_file, &material.key_pem).expect("write key");

        let loaded = load_or_generate(
            Some(cert_file.to_str().unwrap()),
            Some(key_file.to_str().unwrap()),
            &[],
        )
        .expect("load");
        assert_eq!(loaded.cert_pem, material.cert_pem);
        assert_eq!(loaded.key_pem, material.key_pem);
        assert!(!loaded.self_signed);
    }

    #[test]
    fn a_missing_operator_certificate_says_which_file_it_could_not_read() {
        let err =
            load_or_generate(Some("no-such-cert.pem"), Some("no-such-key.pem"), &[]).unwrap_err();
        assert!(err.to_string().contains("no-such-cert.pem"), "{err}");
    }
}
