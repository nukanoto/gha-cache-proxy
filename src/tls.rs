//! Built-in local CA generation and TLS server for intercept mode.
//!
//! Generates (or reloads from `TLS_DATA_DIR`) an internal root CA and signs a
//! leaf server certificate covering the configured GitHub Actions hostnames.
//! Only the public root CA certificate is written to `TLS_CA_CERT_OUT`; the CA
//! private key never leaves the process (or `TLS_DATA_DIR`).

use std::fs;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoConnBuilder;
use hyper_util::service::TowerToHyperService;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use time::{Duration, OffsetDateTime};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

pub struct TlsMaterial {
    pub ca_cert_pem: String,
    pub server_config: Arc<ServerConfig>,
}

pub fn load_or_generate(
    hosts: &[String],
    data_dir: Option<&Path>,
    ca_cert_out: Option<&Path>,
) -> Result<TlsMaterial, String> {
    if hosts.is_empty() {
        return Err("TLS_HOSTS must contain at least one hostname".to_string());
    }

    let _ = rustls::crypto::ring::default_provider().install_default();

    let (ca_cert, ca_key, ca_cert_pem, ca_cert_der) = match data_dir {
        Some(dir) => load_or_create_persisted_ca(dir)?,
        None => create_ephemeral_ca()?,
    };

    if let Some(out_path) = ca_cert_out {
        write_atomic(out_path, ca_cert_pem.as_bytes(), 0o644)?;
    }

    let leaf_key = KeyPair::generate().map_err(|e| format!("generate server key: {e}"))?;
    let mut leaf_params =
        CertificateParams::new(hosts.to_vec()).map_err(|e| format!("leaf cert params: {e}"))?;
    let now = OffsetDateTime::now_utc();
    leaf_params.not_before = now - Duration::hours(1);
    leaf_params.not_after = now + Duration::days(3650);
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, hosts[0].clone());
    leaf_params.distinguished_name = dn;

    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .map_err(|e| format!("sign server cert: {e}"))?;

    let cert_chain = vec![leaf_cert.der().clone(), ca_cert_der];
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));

    let mut server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key_der)
        .map_err(|e| format!("build rustls ServerConfig: {e}"))?;
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(TlsMaterial {
        ca_cert_pem,
        server_config: Arc::new(server_config),
    })
}

fn create_ephemeral_ca() -> Result<(Certificate, KeyPair, String, CertificateDer<'static>), String> {
    let ca_key = KeyPair::generate().map_err(|e| format!("generate CA key: {e}"))?;
    let mut params = CertificateParams::default();
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(1);
    params.not_after = now + Duration::days(3650);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "gha-cache-server Local CA");
    params.distinguished_name = dn;

    let ca_cert = params
        .self_signed(&ca_key)
        .map_err(|e| format!("self-sign CA cert: {e}"))?;
    let ca_pem = ca_cert.pem();
    let ca_der = ca_cert.der().clone();
    Ok((ca_cert, ca_key, ca_pem, ca_der))
}

fn load_or_create_persisted_ca(
    dir: &Path,
) -> Result<(Certificate, KeyPair, String, CertificateDer<'static>), String> {
    fs::create_dir_all(dir).map_err(|e| format!("create TLS_DATA_DIR {}: {e}", dir.display()))?;
    let cert_path = dir.join("ca.crt");
    let key_path = dir.join("ca.key");

    if cert_path.exists() && key_path.exists() {
        let ca_pem = fs::read_to_string(&cert_path)
            .map_err(|e| format!("read {}: {e}", cert_path.display()))?;
        let key_pem = fs::read_to_string(&key_path)
            .map_err(|e| format!("read {}: {e}", key_path.display()))?;
        let ca_key = KeyPair::from_pem(&key_pem)
            .map_err(|e| format!("parse {}: {e}", key_path.display()))?;
        let ca_params = CertificateParams::from_ca_cert_pem(&ca_pem)
            .map_err(|e| format!("parse {}: {e}", cert_path.display()))?;
        let ca_cert = ca_params
            .self_signed(&ca_key)
            .map_err(|e| format!("rebuild CA issuer from {}: {e}", cert_path.display()))?;
        let ca_der = CertificateDer::from_pem_slice(ca_pem.as_bytes())
            .map_err(|e| format!("decode DER from {}: {e}", cert_path.display()))?;
        tracing::info!("loaded persisted local CA from {}", dir.display());
        return Ok((ca_cert, ca_key, ca_pem, ca_der));
    }

    let (ca_cert, ca_key, ca_pem, ca_der) = create_ephemeral_ca()?;
    write_atomic(&key_path, ca_key.serialize_pem().as_bytes(), 0o600)?;
    write_atomic(&cert_path, ca_pem.as_bytes(), 0o644)?;
    tracing::info!("generated new local CA in {}", dir.display());
    Ok((ca_cert, ca_key, ca_pem, ca_der))
}

fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
        }
    }
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("cert"),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);
    fs::write(&tmp_path, data).map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {}: {e}", tmp_path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    fs::rename(&tmp_path, path)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp_path.display(), path.display()))?;
    Ok(())
}

pub async fn serve(
    listener: TcpListener,
    server_config: Arc<ServerConfig>,
    app: Router,
) -> Result<(), String> {
    let acceptor = TlsAcceptor::from(server_config);
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .map_err(|e| format!("TLS accept error: {e}"))?;
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(peer = %peer, error = %e, "TLS handshake failed");
                    return;
                }
            };
            let io = TokioIo::new(tls_stream);
            let service = TowerToHyperService::new(app);
            let builder = AutoConnBuilder::new(TokioExecutor::new());
            if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
                tracing::debug!(peer = %peer, error = %e, "TLS connection error");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::ClientConfig;
    use rustls::RootCertStore;
    use rustls_pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;

    #[tokio::test]
    async fn persisted_ca_verifies_after_reload() {
        let dir = std::env::temp_dir().join(format!("gha-cache-tls-test-{}", uuid::Uuid::new_v4()));
        let out = dir.join("export/root.crt");
        let hosts = vec![
            "artifactcache.actions.githubusercontent.com".to_string(),
            "results-receiver.actions.githubusercontent.com".to_string(),
        ];

        let first = load_or_generate(&hosts, Some(&dir), Some(&out)).expect("first gen");
        let exported = fs::read_to_string(&out).expect("read exported cert");
        assert_eq!(exported, first.ca_cert_pem);

        // Reload CA from disk and generate a new leaf cert; the original exported CA must trust it.
        let second = load_or_generate(&hosts, Some(&dir), Some(&out)).expect("reload");
        assert_eq!(second.ca_cert_pem, first.ca_cert_pem);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(second.server_config);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(stream).await.unwrap();
            tls.write_all(b"ok").await.unwrap();
            tls.shutdown().await.unwrap();
        });

        let mut roots = RootCertStore::empty();
        let ca_der = CertificateDer::from_pem_slice(exported.as_bytes()).unwrap();
        roots.add(ca_der).unwrap();
        let client_cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_cfg));
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let server_name =
            ServerName::try_from("artifactcache.actions.githubusercontent.com").unwrap();
        let mut tls = connector.connect(server_name, tcp).await.unwrap();
        let mut buf = Vec::new();
        tls.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"ok");
        server_task.await.unwrap();

        let _ = fs::remove_dir_all(&dir);
    }
}
