use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::{fs::File, io::BufReader};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use sha2::{Digest, Sha256};
use thiserror::Error;
use webrtc_dtls::{cipher_suite::CipherSuiteId, crypto::Certificate};

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
}

/// The only DTLS cipher suite taken, both ways (0.54.0): ECDHE with AES-128-GCM.
/// The defaults of webrtc-dtls also offered AES-256-CBC with SHA-1, never
/// chosen between two RustInputs nor with Lan Mouse (both put AES-GCM first,
/// and the handshake protects that choice), but one less to trust. Older
/// versions and Lan Mouse offer AES-GCM, so they still connect. The
/// certificates are ECDSA (webrtc-dtls takes no RSA key), hence no RSA suite.
pub(crate) const DTLS_CIPHER_SUITES: [CipherSuiteId; 1] =
    [CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256];

pub fn generate_fingerprint(cert: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(cert);
    let bytes = hash
        .finalize()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>();
    bytes.join(":").to_lowercase()
}

pub fn certificate_fingerprint(cert: &Certificate) -> String {
    let certificate = cert.certificate.first().expect("certificate missing");
    generate_fingerprint(certificate)
}

/// load certificate from file
pub fn load_certificate(path: &Path) -> Result<Certificate, Error> {
    let f = File::open(path)?;

    let mut reader = BufReader::new(f);
    let mut pem = String::new();
    reader.read_to_string(&mut pem)?;
    Ok(Certificate::from_pem(pem.as_str())?)
}

pub(crate) fn load_or_generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    if path.exists() && path.is_file() {
        Ok(load_certificate(path)?)
    } else {
        generate_key_and_cert(path)
    }
}

pub(crate) fn generate_key_and_cert(path: &Path) -> Result<Certificate, Error> {
    let cert = Certificate::generate_self_signed(["ignored".to_owned()])?;
    let serialized = cert.serialize_pem();
    let parent = path.parent().expect("is a path");
    fs::create_dir_all(parent)?;
    // never into a file that is there, which could be a link to another one
    let f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    #[cfg(unix)]
    {
        let mut perm = f.metadata()?.permissions();
        perm.set_mode(0o400); /* r-- --- --- */
        f.set_permissions(perm)?;
    }
    /* FIXME windows permissions */
    let mut writer = BufWriter::new(f);
    writer.write_all(serialized.as_bytes())?;
    Ok(cert)
}
