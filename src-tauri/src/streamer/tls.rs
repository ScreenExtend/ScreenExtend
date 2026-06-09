use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;

const DEV_CERT_FILE: &str = "self-signed-cert.pem";
const DEV_KEY_FILE: &str = "self-signed-key.pem";

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
            let cert_pem = std::fs::read(c)
                .with_context(|| format!("reading TLS cert from {c}"))?;
            let key_pem = std::fs::read(k)
                .with_context(|| format!("reading TLS key from {k}"))?;
            println!("TLS: using operator-provided certificate (cert={c}, key={k})");
            Ok(TlsMaterial { cert_pem, key_pem, self_signed: false })
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
    let cert_file = PathBuf::from(DEV_CERT_FILE);
    let key_file = PathBuf::from(DEV_KEY_FILE);

    if cert_file.exists() && key_file.exists() {
        if let (Ok(cert_pem), Ok(key_pem)) =
            (std::fs::read(&cert_file), std::fs::read(&key_file))
        {
            println!(
                "TLS: reusing cached dev self-signed certificate ({})",
                cert_file.display()
            );
            return Ok(TlsMaterial { cert_pem, key_pem, self_signed: true });
        }
    }

    let sans = subject_alt_names(extra_sans);
    println!("TLS: generating dev self-signed certificate (SANs: {sans:?})");

    let mut params = rcgen::CertificateParams::new(sans)
        .context("building self-signed certificate params")?;
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
        eprintln!("TLS: could not cache dev cert ({e}); it will be regenerated next launch");
    }

    Ok(TlsMaterial { cert_pem, key_pem, self_signed: true })
}

fn cache_dev_cert(cert_file: &Path, cert_pem: &[u8], key_file: &Path, key_pem: &[u8]) -> Result<()> {
    std::fs::write(cert_file, cert_pem).with_context(|| format!("writing {}", cert_file.display()))?;
    std::fs::write(key_file, key_pem).with_context(|| format!("writing {}", key_file.display()))?;
    Ok(())
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
