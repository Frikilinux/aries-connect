//! Self-signed certificate generation, persistence and TLS client setup.
//!
//! KDE Connect identifies devices by a self-signed certificate; the same
//! certificate must be reused across restarts so its fingerprint stays
//! stable. It is persisted to `$XDG_DATA_HOME/aries-connect/{cert.pem,key.pem}`.
//!
//! Trust does not come from a CA chain: peers exchange certificates during
//! pairing and pin them afterwards. Therefore the TLS client below presents
//! our certificate and accepts any peer certificate (verification is
//! delegated to the pairing flow).

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::{
    pem::PemObject, CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime,
};

#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("io failure: {0}")]
    Io(#[from] io::Error),
    #[error("RSA key generation failed: {0}")]
    Rsa(#[from] rsa::Error),
    #[error("pkcs8 encoding failed: {0}")]
    RsaPkcs8(#[from] rsa::pkcs8::Error),
    #[error("certificate generation failed: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("invalid persisted key material")]
    BadPem,
}

/// Material handed to the TLS configuration.
pub struct IdentityCertificate {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
}

impl IdentityCertificate {
    /// Loads the persisted certificate or creates a fresh RSA-2048 one.
    pub async fn load_or_create(data_dir: PathBuf) -> Result<Self, CertError> {
        fs::create_dir_all(&data_dir)?;
        let cert_path = data_dir.join("cert.pem");
        let key_path = data_dir.join("key.pem");

        if cert_path.is_file() && key_path.is_file() {
            match Self::load(&cert_path, &key_path) {
                Ok(cert) => {
                    tracing::info!(dir = %data_dir.display(), "loaded existing TLS certificate");
                    return Ok(cert);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "certificate unreadable, regenerating");
                }
            }
        }

        let cert = tokio::task::spawn_blocking(move || Self::generate(&cert_path, &key_path))
            .await
            .expect("blocking task cannot panic")?;
        tracing::info!(dir = %data_dir.display(), "generated new TLS certificate");
        Ok(cert)
    }

    fn load(cert_path: &PathBuf, key_path: &PathBuf) -> Result<Self, CertError> {
        let mut certs = CertificateDer::pem_file_iter(cert_path)
            .map_err(|_| CertError::BadPem)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| CertError::BadPem)?;
        let cert = certs.pop().ok_or(CertError::BadPem)?;
        let key = PrivateKeyDer::pem_file_iter(key_path)
            .map_err(|_| CertError::BadPem)?
            .next()
            .ok_or(CertError::BadPem)?
            .map_err(|_| CertError::BadPem)?;
        Ok(Self { cert, key })
    }

    fn generate(cert_path: &PathBuf, key_path: &PathBuf) -> Result<Self, CertError> {
        // RSA 2048: what the Android app and the reference client expect from
        // a KDE Connect identity certificate.
        let mut rng = rand::thread_rng();
        let rsa_key = rsa::RsaPrivateKey::new(&mut rng, 2048)?;
        let pkcs8_bytes: Vec<u8> = rsa_key.to_pkcs8_der()?.to_bytes().to_vec();

        let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(pkcs8_bytes.clone()),
            &rcgen::PKCS_RSA_SHA256,
        )?;
        let params = rcgen::CertificateParams::new(vec![])?;
        let cert = params.self_signed(&key_pair)?;

        // Persist so the fingerprint is stable across restarts.
        fs::write(cert_path, cert.pem())?;
        let key_pem = rsa_key.to_pkcs8_pem(LineEnding::LF)?;
        fs::write(key_path, key_pem.as_bytes())?;

        Ok(Self {
            cert: CertificateDer::from(cert.der().to_vec()),
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8_bytes)),
        })
    }
}

/// Accepts any peer certificate. KDE Connect pins certificates during the
/// pairing exchange instead of relying on a CA chain.
#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
        ]
    }
}

/// TLS client configuration used for every control channel: present our
/// certificate (client auth) and accept the peer's self-signed one.
pub fn client_config(cert: &IdentityCertificate) -> io::Result<ClientConfig> {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_client_auth_cert(vec![cert.cert.clone()], cert.key.clone_key())
        .map_err(io::Error::other)?;
    Ok(config)
}
