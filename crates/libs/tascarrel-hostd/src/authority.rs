use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use rcgen::BasicConstraints;
use rcgen::CertificateParams;
use rcgen::DistinguishedName;
use rcgen::DnType;
use rcgen::IsCa;
use rcgen::Issuer;
use rcgen::KeyPair;
use rcgen::KeyUsagePurpose;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::PrivatePkcs8KeyDer;

const KEY_FILE: &str = "ca-key.pem";
const CERTIFICATE_FILE: &str = "ca.pem";
const MAX_KEY_BYTES: u64 = 64 * 1024;
const MAX_CERTIFICATE_BYTES: u64 = 64 * 1024;

/// Persistent workspace CA used only for explicitly enabled HTTPS inspection.
pub struct WorkspaceAuthority {
    issuer: Issuer<'static, KeyPair>,
    pem: Arc<[u8]>,
    der: CertificateDer<'static>,
}

impl std::fmt::Debug for WorkspaceAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceAuthority")
            .field("pem_bytes", &self.pem.len())
            .field("signing_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl WorkspaceAuthority {
    /// Opens or creates the private signing key below workspace state.
    ///
    /// # Errors
    ///
    /// Returns an error when the state path is unsafe, cannot be read or
    /// written, or contains invalid CA material.
    pub fn load_or_create(directory: &Path, workspace: &str) -> Result<Arc<Self>> {
        ensure_private_directory(directory)?;
        let key = load_or_create_key(&directory.join(KEY_FILE))?;
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(
            DnType::CommonName,
            format!("Tascarrel workspace {workspace}"),
        );
        let mut params = CertificateParams::default();
        params.distinguished_name = distinguished_name;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::CrlSign,
        ];
        let pem = load_or_create_certificate(&directory.join(CERTIFICATE_FILE), &params, &key)?;
        let issuer = Issuer::from_ca_cert_pem(&pem, key).context("load workspace CA")?;
        let certificates = rustls_pemfile::certs(&mut Cursor::new(pem.as_bytes()))
            .collect::<std::io::Result<Vec<_>>>()
            .context("parse workspace CA certificate")?;
        let [der] = certificates.as_slice() else {
            bail!("workspace CA file must contain exactly one certificate");
        };
        let der = der.clone();
        let pem: Arc<[u8]> = Arc::from(pem.into_bytes());
        Ok(Arc::new(Self { issuer, pem, der }))
    }

    #[must_use]
    pub fn certificate_pem(&self) -> Arc<[u8]> {
        Arc::clone(&self.pem)
    }

    #[must_use]
    pub(crate) fn certificate_der(&self) -> CertificateDer<'static> {
        self.der.clone()
    }

    /// Issues an ephemeral leaf certificate and TLS server configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid DNS name or certificate/key failure.
    pub fn server_config(&self, host: &str) -> Result<Arc<rustls::ServerConfig>> {
        let key = KeyPair::generate().context("generate HTTPS leaf key")?;
        let certificate = CertificateParams::new(vec![host.to_owned()])?
            .signed_by(&key, &self.issuer)
            .context("sign HTTPS leaf certificate")?;
        let chain = vec![CertificateDer::from(certificate.der().to_vec())];
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, private_key)
            .context("load HTTPS leaf certificate")?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            bail!(
                "workspace CA state is not a real directory: {}",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .with_context(|| format!("create workspace CA directory {}", path.display()))?;
        }
        Err(error) => return Err(error).context("inspect workspace CA directory"),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn load_or_create_key(path: &Path) -> Result<KeyPair> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    match options.open(path) {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() > MAX_KEY_BYTES {
                bail!("workspace CA key is not a bounded regular file");
            }
            let mut pem = String::new();
            file.read_to_string(&mut pem)?;
            KeyPair::from_pem(&pem).context("parse workspace CA key")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_key(path),
        Err(error) => Err(error).context("open workspace CA key"),
    }
}

fn create_key(path: &Path) -> Result<KeyPair> {
    let key = KeyPair::generate().context("generate workspace CA key")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("create workspace CA key {}", path.display()))?;
    file.write_all(key.serialize_pem().as_bytes())?;
    file.sync_all()?;
    Ok(key)
}

fn load_or_create_certificate(
    path: &Path,
    params: &CertificateParams,
    key: &KeyPair,
) -> Result<String> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    match options.open(path) {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() > MAX_CERTIFICATE_BYTES {
                bail!("workspace CA certificate is not a bounded regular file");
            }
            let mut pem = String::new();
            file.read_to_string(&mut pem)?;
            Ok(pem)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let certificate = params
                .clone()
                .self_signed(key)
                .context("create workspace CA")?;
            let pem = certificate.pem();
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
                .open(path)
                .with_context(|| format!("create workspace CA certificate {}", path.display()))?;
            file.write_all(pem.as_bytes())?;
            file.sync_all()?;
            Ok(pem)
        }
        Err(error) => Err(error).context("open workspace CA certificate"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies host certificates remain stable across authority reloads.
    #[test]
    fn authority_persists_its_key_and_issues_host_certificates() {
        if rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .is_err()
        {
            tracing::debug!("rustls crypto provider was already installed");
        }
        let directory = tempfile::tempdir().unwrap();
        let first = WorkspaceAuthority::load_or_create(directory.path(), "demo").unwrap();
        let first_pem = first.certificate_pem();
        assert!(first.server_config("api.example").is_ok());
        drop(first);
        let second = WorkspaceAuthority::load_or_create(directory.path(), "demo").unwrap();
        assert_eq!(&*first_pem, &*second.certificate_pem());
        assert_eq!(
            fs::metadata(directory.path().join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(directory.path().join(CERTIFICATE_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }
}
