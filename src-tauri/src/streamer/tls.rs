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

    if cert_file.exists() && key_file.exists() {
        if let (Ok(cert_pem), Ok(key_pem)) = (std::fs::read(&cert_file), std::fs::read(&key_file)) {
            tprintln!(
                "TLS: reusing cached dev self-signed certificate ({})",
                cert_file.display()
            );
            return Ok(TlsMaterial {
                cert_pem,
                key_pem,
                self_signed: true,
            });
        }
    }

    let sans = subject_alt_names(extra_sans);
    tprintln!("TLS: generating dev self-signed certificate (SANs: {sans:?})");

    let mut params =
        rcgen::CertificateParams::new(sans).context("building self-signed certificate params")?;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "ScreenExtend");
    params.distinguished_name = dn;

    let key_pair = rcgen::KeyPair::generate().context("generating certificate key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("generating self-signed certificate")?;
    let cert_pem = cert.pem().into_bytes();
    let key_pem = key_pair.serialize_pem().into_bytes();

    if let Err(e) = cache_dev_cert(&cert_file, &cert_pem, &key_file, &key_pem) {
        teprintln!("TLS: could not cache dev cert ({e}); it will be regenerated next launch");
    }

    Ok(TlsMaterial {
        cert_pem,
        key_pem,
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

    #[test]
    fn the_san_list_always_carries_the_two_the_client_can_reach_it_by() {
        // Every join lands on either the loopback name or an adapter address, so both have to be
        // in the certificate or the HTTPS upsell produces a name-mismatch warning instead of the
        // self-signed one the client page knows how to explain.
        let sans = subject_alt_names(&[]);
        assert!(sans.contains(&"localhost".to_string()));
        assert!(sans.contains(&"127.0.0.1".to_string()));
    }

    #[test]
    fn adapter_addresses_are_added_once_and_empties_are_ignored() {
        let sans = subject_alt_names(&[
            "192.168.1.20".to_string(),
            "192.168.1.20".to_string(),
            String::new(),
            "10.0.0.5".to_string(),
        ]);
        assert_eq!(
            sans,
            vec!["localhost", "127.0.0.1", "192.168.1.20", "10.0.0.5"]
        );
    }

    #[test]
    fn a_duplicate_of_a_built_in_name_does_not_repeat_it() {
        assert_eq!(
            subject_alt_names(&["127.0.0.1".to_string()]),
            vec!["localhost", "127.0.0.1"]
        );
    }

    #[test]
    fn one_half_of_an_operator_certificate_is_an_error_not_a_silent_self_signed_fallback() {
        // Falling back would mean an operator who typo'd one path gets a self-signed cert and a
        // browser warning, with the log claiming TLS is configured.
        assert!(load_or_generate(Some("cert.pem"), None, &[])
            .map(|_| ())
            .is_err());
        assert!(load_or_generate(None, Some("key.pem"), &[])
            .map(|_| ())
            .is_err());
    }

    #[test]
    fn a_missing_operator_certificate_names_the_path_it_could_not_read() {
        let err = match load_or_generate(
            Some("definitely-not-here-cert.pem"),
            Some("definitely-not-here-key.pem"),
            &[],
        ) {
            Ok(_) => panic!("a missing certificate must not load"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("definitely-not-here-cert.pem"),
            "the error must name the file: {err:#}"
        );
    }

    /// The generated material has to be something `rustls` will actually load, which is the one
    /// property the rest of the module cannot check for itself.
    #[test]
    fn a_generated_dev_certificate_is_loadable_and_is_reused_from_cache() {
        let dir = std::env::temp_dir().join(format!("se-tls-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cert = dir.join(DEV_CERT_FILE);
        let key = dir.join(DEV_KEY_FILE);
        let _ = std::fs::remove_file(&cert);
        let _ = std::fs::remove_file(&key);

        let sans = subject_alt_names(&["192.168.1.20".to_string()]);
        let params = rcgen::CertificateParams::new(sans).expect("params");
        let key_pair = rcgen::KeyPair::generate().expect("key pair");
        let generated = params.self_signed(&key_pair).expect("self-signed");
        let cert_pem = generated.pem().into_bytes();
        let key_pem = key_pair.serialize_pem().into_bytes();

        assert!(cert_pem.starts_with(b"-----BEGIN CERTIFICATE-----"));
        assert!(String::from_utf8_lossy(&key_pem).contains("PRIVATE KEY"));

        cache_dev_cert(&cert, &cert_pem, &key, &key_pem).expect("cache");
        assert_eq!(std::fs::read(&cert).unwrap(), cert_pem);
        assert_eq!(std::fs::read(&key).unwrap(), key_pem);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
