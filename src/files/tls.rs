//! TLS of the file channel, with the same certificate as the input channel.
//!
//! Certificates are self signed, so the usual chain validation does not
//! apply: a connection is trusted by the fingerprint of the certificate
//! instead, like the input channel does. The accepting side only lets in
//! authorized fingerprints, the connecting side only talks to the exact
//! fingerprint of the device it already talks to over the input channel.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, Error, ServerConfig, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use webrtc_dtls::crypto::Certificate;

use crate::crypto;

/// fingerprints authorized on this device, with their descriptions
pub(crate) type AuthorizedKeys = Arc<RwLock<HashMap<String, String>>>;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn identity(cert: &Certificate) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let key = PrivatePkcs8KeyDer::from(cert.private_key.serialized_der.clone());
    (cert.certificate.clone(), PrivateKeyDer::Pkcs8(key))
}

/// configuration of the accepting side
pub(crate) fn server_config(
    cert: &Certificate,
    authorized: AuthorizedKeys,
) -> Result<Arc<ServerConfig>, Error> {
    let provider = provider();
    let verifier = AuthorizedClients {
        authorized,
        provider: provider.clone(),
    };
    let (chain, key) = identity(cert);
    let config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(chain, key)?;
    Ok(Arc::new(config))
}

/// configuration of the connecting side, talking to `fingerprint` only
pub(crate) fn client_config(
    cert: &Certificate,
    fingerprint: String,
) -> Result<Arc<ClientConfig>, Error> {
    let provider = provider();
    let verifier = PinnedServer {
        fingerprint,
        provider: provider.clone(),
    };
    let (chain, key) = identity(cert);
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(chain, key)?;
    Ok(Arc::new(config))
}

/// the name the connecting side asks for, certificates are not checked by name
pub(crate) fn server_name() -> ServerName<'static> {
    ServerName::try_from("rustinput").expect("valid name")
}

/// the fingerprint of the certificate the peer presented
pub(crate) fn peer_fingerprint(certs: Option<&[CertificateDer<'_>]>) -> Option<String> {
    certs
        .and_then(|c| c.first())
        .map(|c| crypto::generate_fingerprint(c))
}

fn signature_schemes(provider: &CryptoProvider) -> Vec<SignatureScheme> {
    provider
        .signature_verification_algorithms
        .supported_schemes()
}

#[derive(Debug)]
struct AuthorizedClients {
    authorized: AuthorizedKeys,
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AuthorizedClients {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        let fingerprint = crypto::generate_fingerprint(end_entity);
        if self
            .authorized
            .read()
            .expect("lock")
            .contains_key(&fingerprint)
        {
            Ok(ClientCertVerified::assertion())
        } else {
            log::info!("file channel: refused the unauthorized device {fingerprint}");
            Err(Error::General("device not authorized".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        signature_schemes(&self.provider)
    }
}

#[derive(Debug)]
struct PinnedServer {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if crypto::generate_fingerprint(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::General(
                "the device answering is not the one of the input channel".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        signature_schemes(&self.provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    fn cert() -> Certificate {
        Certificate::generate_self_signed(["ignored".to_owned()]).expect("certificate")
    }

    async fn handshake(
        server: &Certificate,
        authorized: &[&Certificate],
        client: &Certificate,
        pinned: &Certificate,
    ) -> Result<String, String> {
        let keys: HashMap<_, _> = authorized
            .iter()
            .map(|c| (crypto::certificate_fingerprint(c), "peer".to_owned()))
            .collect();
        let acceptor = TlsAcceptor::from(
            server_config(server, Arc::new(RwLock::new(keys))).expect("server config"),
        );
        let connector = TlsConnector::from(
            client_config(client, crypto::certificate_fingerprint(pinned)).expect("client config"),
        );
        let (a, b) = tokio::io::duplex(64 * 1024);
        let accept = async move {
            let mut stream = acceptor.accept(a).await.map_err(|e| e.to_string())?;
            let fingerprint = peer_fingerprint(stream.get_ref().1.peer_certificates());
            stream.write_all(b"ok").await.map_err(|e| e.to_string())?;
            stream.flush().await.map_err(|e| e.to_string())?;
            fingerprint.ok_or_else(|| "no client certificate".to_owned())
        };
        let connect = async move {
            let mut stream = connector
                .connect(server_name(), b)
                .await
                .map_err(|e| e.to_string())?;
            let mut reply = [0; 2];
            stream
                .read_exact(&mut reply)
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>(())
        };
        let (accepted, connected) = tokio::join!(accept, connect);
        connected?;
        accepted
    }

    #[tokio::test]
    async fn authorized_devices_connect() {
        let (server, client) = (cert(), cert());
        let fingerprint = handshake(&server, &[&client], &client, &server)
            .await
            .expect("handshake");
        assert_eq!(fingerprint, crypto::certificate_fingerprint(&client));
    }

    #[tokio::test]
    async fn unauthorized_clients_are_refused() {
        let (server, client, other) = (cert(), cert(), cert());
        assert!(
            handshake(&server, &[&other], &client, &server)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_different_server_is_refused() {
        let (server, client, expected) = (cert(), cert(), cert());
        assert!(
            handshake(&server, &[&client], &client, &expected)
                .await
                .is_err()
        );
    }
}
