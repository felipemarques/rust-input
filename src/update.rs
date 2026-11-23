//! Asking GitHub for the latest version of RustInput, when the user wants it
//! (`check_updates`, asked in the window).
//!
//! One HEAD request for the page of the latest release, which GitHub
//! answers with a redirect to the page of its tag: the version is in the
//! address. Nothing is sent but the request itself (with `RustInput/<version>`
//! as the program asking); no API, so no limit of requests and no JSON.

use std::{sync::Arc, time::Duration};

use rustls::pki_types::ServerName;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;

const HOST: &str = "github.com";
const LATEST_PATH: &str = "/felipemarques/rust-input/releases/latest";
/// the page of the latest release, for the user
pub(crate) const RELEASES_PAGE: &str =
    "https://github.com/felipemarques/rust-input/releases/latest";
/// the whole question, connecting included
const TIMEOUT: Duration = Duration::from_secs(30);
/// the head of the answer, at most
const MAX_HEAD: usize = 64 * 1024;

/// The latest version of RustInput on GitHub ("0.27.0"), or why it is not
/// known.
pub(crate) async fn latest() -> Result<String, String> {
    // the end-to-end tests pretend a version is out, without GitHub; only
    // in debug builds
    #[cfg(debug_assertions)]
    if let Ok(version) = std::env::var("RUSTINPUT_TEST_LATEST") {
        return Ok(version);
    }
    timeout(TIMEOUT, ask())
        .await
        .map_err(|_| "GitHub did not answer".to_owned())?
}

async fn ask() -> Result<String, String> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for error in &native.errors {
        log::debug!("certificates of the system: {error}");
    }
    let (added, _) = roots.add_parsable_certificates(native.certs);
    if added == 0 {
        return Err("no certificates of the system to check GitHub with".into());
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let server = ServerName::try_from(HOST).map_err(|e| e.to_string())?;
    let tcp = TcpStream::connect((HOST, 443))
        .await
        .map_err(|e| format!("{HOST}: {e}"))?;
    let mut tls = TlsConnector::from(Arc::new(config))
        .connect(server, tcp)
        .await
        .map_err(|e| format!("{HOST}: {e}"))?;
    let request = format!(
        "HEAD {LATEST_PATH} HTTP/1.1\r\nHost: {HOST}\r\nUser-Agent: RustInput/{}\r\n\
         Accept: text/html\r\nConnection: close\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    );
    tls.write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < MAX_HEAD {
        match tls.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
            // GitHub may close without saying so, after the head
            Err(_) if !head.is_empty() => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    version_of(&String::from_utf8_lossy(&head))
}

/// the version in the redirect GitHub answers with
fn version_of(head: &str) -> Result<String, String> {
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default();
    let code = status.split_whitespace().nth(1).unwrap_or_default();
    if !code.starts_with('3') {
        return Err(format!("unexpected answer from GitHub: {}", status.trim()));
    }
    let location = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("location"))
        .map(|(_, value)| value.trim())
        .ok_or("GitHub did not tell the latest release")?;
    let tag = location
        .rsplit_once("/releases/tag/")
        .map(|(_, tag)| tag.trim_end_matches('/'))
        .ok_or_else(|| format!("no release in {location}"))?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    parse(version).ok_or_else(|| format!("not a version: {tag}"))?;
    Ok(version.to_owned())
}

/// the numbers of a version "0.27.0"
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// whether `latest` is newer than `current`
pub(crate) fn newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_from_the_redirect() {
        let head = "HTTP/1.1 302 Found\r\nServer: GitHub.com\r\n\
                    location: https://github.com/felipemarques/rust-input/releases/tag/v0.26.0\r\n\r\n";
        assert_eq!(version_of(head), Ok("0.26.0".to_owned()));
        assert!(version_of("HTTP/1.1 404 Not Found\r\n\r\n").is_err());
        assert!(
            version_of("HTTP/1.1 302 Found\r\nLocation: https://github.com/login\r\n\r\n").is_err()
        );
    }

    #[test]
    fn newer_by_the_numbers() {
        assert!(newer("0.27.0", "0.26.0"));
        assert!(newer("0.26.10", "0.26.9"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.26.0", "0.26.0"));
        assert!(!newer("0.25.1", "0.26.0"));
        assert!(!newer("garbage", "0.26.0"));
    }
}
