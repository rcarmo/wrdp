//! TLS certificate loading for the production listener.

use anyhow::{Context, Result};
use std::{fs::File, io::BufReader, path::Path, sync::Arc};
use tokio_rustls::rustls::{
    self, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer},
};

#[derive(Clone)]
pub struct TlsConfig {
    server_config: Arc<ServerConfig>,
}
impl TlsConfig {
    pub fn from_files(cert: &Path, key: &Path) -> Result<Self> {
        Self::from_files_with_options(cert, key, false)
    }
    pub fn from_files_with_options(cert: &Path, key: &Path, tls13_only: bool) -> Result<Self> {
        let certificates = certificates(cert)?;
        let private_key = private_key(key)?;
        let builder = if tls13_only {
            ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        } else {
            ServerConfig::builder()
        };
        let configured = builder
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)
            .context("certificate and private key are incompatible")?;
        Ok(Self {
            server_config: Arc::new(configured),
        })
    }
    pub fn server_config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.server_config)
    }
}
fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let mut input = BufReader::new(
        File::open(path).with_context(|| format!("open certificate {}", path.display()))?,
    );
    let chain = rustls_pemfile::certs(&mut input)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse certificate PEM")?;
    anyhow::ensure!(
        !chain.is_empty(),
        "certificate file contains no certificates"
    );
    Ok(chain)
}
fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let mut input = BufReader::new(
        File::open(path).with_context(|| format!("open private key {}", path.display()))?,
    );
    rustls_pemfile::private_key(&mut input)
        .context("parse private-key PEM")?
        .context("private-key file contains no key")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_certificate_has_path_context() {
        let e = certificates(Path::new("/missing/wrdp.pem")).unwrap_err();
        assert!(e.to_string().contains("open certificate"));
    }
}
