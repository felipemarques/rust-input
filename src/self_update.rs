//! Installing a new version of RustInput with one click, on Linux (0.51.0).
//!
//! Every release publishes `SHA256SUMS` (the files for Linux and macOS) and
//! `SHA256SUMS.sig`, its Ed25519 signature with the release key, whose
//! public half is built in (`build-aux/release-key.pub`). Installing:
//!
//! 1. both are downloaded from the release of the version and the signature
//!    is checked against the built-in key: HTTPS proves they came from
//!    GitHub, the signature that they came from the owner of RustInput;
//! 2. the file for this installation is downloaded beside the one it
//!    replaces, and its SHA-256 compared with its line;
//! 3. it takes the place of the running executable or AppImage (the old one
//!    is kept as `<name>.previous`), or the package of Debian is installed
//!    with `pkexec apt-get`, which asks for the password of an
//!    administrator.
//!
//! Nothing unchecked is ever run, and a failure leaves RustInput as it was.
//! The new version runs when RustInput starts again ("Restart now").

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use rustinput_ipc::Install;
use rustls::pki_types::ServerName;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::TlsConnector;

/// where the files of the releases are, `/v<version>/<file>`
const RELEASES: &str = "https://github.com/felipemarques/rust-input/releases/download";
/// the public half of the key that signs the checksums of the releases
const RELEASE_KEY: &str = include_str!("../build-aux/release-key.pub");
/// a read that takes longer than this fails the download
const IDLE: Duration = Duration::from_secs(60);
/// the biggest file of a release, and of its checksums
const MAX_FILE: u64 = 1 << 30;
const MAX_SUMS: u64 = 64 * 1024;
/// redirects followed, at most (GitHub sends the file from another host)
const MAX_REDIRECTS: usize = 5;

/// how this RustInput was installed, which decides what replaces it
#[derive(Clone, Debug, PartialEq, Eq)]
// elsewhere than on Unix it never installs itself
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) enum Kind {
    /// an executable the user can replace (`~/.local/bin/rustinput`)
    Binary(PathBuf),
    /// the AppImage it runs from
    AppImage(PathBuf),
    /// the package of Debian (`/usr/bin/rustinput`)
    Deb,
}

impl Kind {
    /// the file of the release for this kind on this computer
    fn file(&self, version: &str) -> Option<String> {
        let (plain, appimage, deb) = match std::env::consts::ARCH {
            "x86_64" => ("x86_64", "x86_64", "amd64"),
            "aarch64" => ("arm64", "aarch64", "arm64"),
            _ => return None,
        };
        Some(match self {
            Kind::Binary(_) => format!("rustinput-linux-{plain}"),
            Kind::AppImage(_) => format!("rustinput_{version}_{appimage}.AppImage"),
            Kind::Deb => format!("rustinput_{version}_{deb}.deb"),
        })
    }
}

/// the running executable, also after an update replaced it (Linux then
/// names it "… (deleted)")
pub(crate) fn current_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let text = exe.to_string_lossy();
    if let Some(path) = text.strip_suffix(" (deleted)") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    Ok(exe)
}

/// what starts RustInput again: the AppImage it runs from, or its
/// executable
pub fn program() -> std::io::Result<PathBuf> {
    match std::env::var_os("APPIMAGE").map(PathBuf::from) {
        Some(appimage) if appimage.is_absolute() && appimage.is_file() => Ok(appimage),
        _ => current_exe(),
    }
}

/// whether this user may write in `dir`
#[cfg(unix)]
fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(dir) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(dir.as_ptr(), libc::W_OK) == 0 }
}

/// How this RustInput can be replaced by a new version, `None` when it
/// cannot do it itself (Flatpak, a folder of the system without a package,
/// another system than Linux).
pub(crate) fn kind() -> Option<Kind> {
    if !cfg!(target_os = "linux") || Path::new("/.flatpak-info").exists() {
        return None;
    }
    #[cfg(unix)]
    {
        if let Some(appimage) = std::env::var_os("APPIMAGE").map(PathBuf::from) {
            let dir = appimage.parent()?;
            return (appimage.is_absolute() && appimage.is_file() && writable(dir))
                .then_some(Kind::AppImage(appimage));
        }
        let exe = current_exe().ok()?;
        if exe == Path::new("/usr/bin/rustinput") {
            let packaged = Path::new("/var/lib/dpkg/info/rustinput.list").is_file();
            let pkexec = Path::new("/usr/bin/pkexec").is_file();
            return (packaged && pkexec).then_some(Kind::Deb);
        }
        let dir = exe.parent()?;
        let named = exe
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("rustinput"));
        (named && writable(dir)).then_some(Kind::Binary(exe))
    }
    #[cfg(not(unix))]
    None
}

/// the built-in public key, or the one of the end-to-end tests (debug
/// builds only)
fn release_key() -> Result<Vec<u8>, String> {
    #[cfg(debug_assertions)]
    if let Ok(key) = std::env::var("RUSTINPUT_TEST_RELEASE_KEY") {
        return hex(&key).ok_or_else(|| "a test key that is not hex".into());
    }
    hex(RELEASE_KEY).ok_or_else(|| "the built-in release key is broken".into())
}

/// where the releases are: GitHub, or the server of the end-to-end tests
/// (debug builds only)
fn releases() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("RUSTINPUT_TEST_RELEASES") {
        return url;
    }
    RELEASES.to_owned()
}

fn hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// whether `signature` is the one of the release key on `message`
fn signed(key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(message, signature)
        .is_ok()
}

/// the SHA-256 of `file` in a `SHA256SUMS` ("<hex>  <name>" per line)
fn checksum(sums: &str, file: &str) -> Option<Vec<u8>> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim_start().trim_start_matches('*');
        (name == file).then(|| hex(hash)).flatten()
    })
}

/// an address `http(s)://host[:port]/path`
#[derive(Debug, PartialEq, Eq)]
struct Url {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

fn url(text: &str) -> Result<Url, String> {
    let (https, rest) = if let Some(rest) = text.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = text.strip_prefix("http://") {
        // only the server of the end-to-end tests speaks plain HTTP
        if !cfg!(debug_assertions) {
            return Err(format!("not HTTPS: {text}"));
        }
        (false, rest)
    } else {
        return Err(format!("not an address: {text}"));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| format!("bad port: {text}"))?),
        None => (authority, if https { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("no host: {text}"));
    }
    Ok(Url {
        https,
        host: host.to_owned(),
        port,
        path: path.to_owned(),
    })
}

/// what takes the body of an answer as it arrives, with its size when known
type Sink<'a> = dyn FnMut(&[u8], u64) -> Result<(), String> + 'a;

/// what a server answered to a GET
enum Answer {
    /// go there
    Redirect(String),
    /// the file went to the sink
    Done,
}

/// A GET of `url` that follows redirects, giving the body to `sink` as it
/// arrives, with its size when known (`total`), at most `max` bytes.
async fn get(url_text: &str, max: u64, sink: &mut Sink<'_>) -> Result<(), String> {
    let mut address = url_text.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let target = url(&address)?;
        let answer = if target.https {
            let config = crate::update::tls_config()?;
            let server = ServerName::try_from(target.host.clone()).map_err(|e| e.to_string())?;
            let tcp = connect(&target).await?;
            let tls = timeout(
                IDLE,
                TlsConnector::from(Arc::new(config)).connect(server, tcp),
            )
            .await
            .map_err(|_| format!("{}: no answer", target.host))?
            .map_err(|e| format!("{}: {e}", target.host))?;
            exchange(tls, &target, max, sink).await?
        } else {
            exchange(connect(&target).await?, &target, max, sink).await?
        };
        match answer {
            Answer::Done => return Ok(()),
            Answer::Redirect(location) => {
                address = if location.starts_with('/') {
                    let scheme = if target.https { "https" } else { "http" };
                    format!("{scheme}://{}:{}{location}", target.host, target.port)
                } else {
                    location
                };
            }
        }
    }
    Err(format!("too many redirects for {url_text}"))
}

async fn connect(target: &Url) -> Result<TcpStream, String> {
    timeout(
        IDLE,
        TcpStream::connect((target.host.as_str(), target.port)),
    )
    .await
    .map_err(|_| format!("{}: no answer", target.host))?
    .map_err(|e| format!("{}: {e}", target.host))
}

/// the request on one connection, and its answer
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    target: &Url,
    max: u64,
    sink: &mut Sink<'_>,
) -> Result<Answer, String> {
    let host = match (target.https, target.port) {
        (true, 443) | (false, 80) => target.host.clone(),
        (_, port) => format!("{}:{port}", target.host),
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: RustInput/{}\r\nAccept: */*\r\n\
         Connection: close\r\n\r\n",
        target.path,
        env!("CARGO_PKG_VERSION")
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut head = Vec::new();
    let end = loop {
        if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if head.len() > 64 * 1024 {
            return Err(format!("{}: an answer without end", target.host));
        }
        let n = read(&mut stream, &mut buf, &target.host).await?;
        if n == 0 {
            return Err(format!("{}: the answer stopped", target.host));
        }
        head.extend_from_slice(&buf[..n]);
    };
    let rest = head.split_off(end);
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default().trim().to_owned();
    let code = status.split_whitespace().nth(1).unwrap_or_default();
    let header = |name: &str| {
        head.lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .find(|(n, _)| n.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_owned())
    };
    if code.starts_with('3') {
        return header("location")
            .map(Answer::Redirect)
            .ok_or_else(|| format!("{}: a redirect to nowhere", target.host));
    }
    if code != "200" {
        return Err(format!("{}{} answered {status}", target.host, target.path));
    }
    let chunked = header("transfer-encoding").is_some_and(|v| v.contains("chunked"));
    let length: Option<u64> = header("content-length").and_then(|v| v.parse().ok());
    let total = length.unwrap_or(0);
    if total > max {
        return Err(format!("{}: {total} bytes is too big", target.host));
    }
    let mut got = 0u64;
    let mut give = |data: &[u8], got: &mut u64| -> Result<(), String> {
        *got += data.len() as u64;
        if *got > max {
            return Err(format!("{}: more than {max} bytes", target.host));
        }
        sink(data, total)
    };
    if chunked {
        let mut pending = rest;
        let mut size: Option<usize> = None;
        loop {
            match size {
                None => {
                    if let Some(i) = pending.windows(2).position(|w| w == b"\r\n") {
                        let line = String::from_utf8_lossy(&pending[..i]).into_owned();
                        let hex_size = line.split(';').next().unwrap_or_default().trim();
                        let n = usize::from_str_radix(hex_size, 16)
                            .map_err(|_| format!("{}: a broken chunk", target.host))?;
                        pending.drain(..i + 2);
                        if n == 0 {
                            return Ok(Answer::Done);
                        }
                        size = Some(n);
                        continue;
                    }
                }
                Some(n) if pending.len() >= n + 2 => {
                    give(&pending[..n], &mut got)?;
                    pending.drain(..n + 2);
                    size = None;
                    continue;
                }
                Some(_) => {}
            }
            let n = read(&mut stream, &mut buf, &target.host).await?;
            if n == 0 {
                return Err(format!("{}: the answer stopped", target.host));
            }
            pending.extend_from_slice(&buf[..n]);
        }
    }
    if !rest.is_empty() {
        give(&rest, &mut got)?;
    }
    loop {
        if length.is_some_and(|l| got >= l) {
            return Ok(Answer::Done);
        }
        let n = read(&mut stream, &mut buf, &target.host).await?;
        if n == 0 {
            return match length {
                Some(l) if got < l => Err(format!("{}: the answer stopped", target.host)),
                _ => Ok(Answer::Done),
            };
        }
        give(&buf[..n], &mut got)?;
    }
}

async fn read<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut [u8],
    host: &str,
) -> Result<usize, String> {
    match timeout(IDLE, stream.read(buf)).await {
        Err(_) => Err(format!("{host}: no answer for {} s", IDLE.as_secs())),
        Ok(Ok(n)) => Ok(n),
        // a server that closes without saying so, as GitHub may
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
        Ok(Err(e)) => Err(format!("{host}: {e}")),
    }
}

/// a small file of the release, whole
async fn small(url: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    get(url, MAX_SUMS, &mut |data, _| {
        out.extend_from_slice(data);
        Ok(())
    })
    .await?;
    Ok(out)
}

/// Downloads `version`, checks it and installs it in place of this one,
/// telling `progress` how it goes; the error says why it did not.
pub(crate) async fn install(
    kind: Kind,
    version: String,
    progress: impl Fn(Install),
) -> Result<(), String> {
    let file = kind
        .file(&version)
        .ok_or("no release for this kind of processor")?;
    let base = format!("{}/v{version}", releases().trim_end_matches('/'));
    progress(Install::Downloading { done: 0, total: 0 });
    let sums = small(&format!("{base}/SHA256SUMS")).await?;
    let signature = small(&format!("{base}/SHA256SUMS.sig"))
        .await
        .map_err(|e| format!("the release has no signature ({e}): update by hand"))?;
    progress(Install::Checking);
    if !signed(&release_key()?, &sums, &signature) {
        return Err("the signature of the release is not the one of RustInput".into());
    }
    let sums = String::from_utf8_lossy(&sums);
    let expected = checksum(&sums, &file).ok_or_else(|| format!("{file} is not in the release"))?;

    // beside the file it replaces, so that renaming replaces it at once
    let target = match &kind {
        Kind::Binary(path) | Kind::AppImage(path) => path.clone(),
        Kind::Deb => {
            let dir = std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
                .ok_or("no folder for the download")?
                .join("rustinput")
                .join("updates");
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            dir.join(&file)
        }
    };
    let dir = target.parent().ok_or("no folder for the download")?;
    let part = dir.join(format!(".{file}.part"));
    let result = download(&format!("{base}/{file}"), &part, &expected, &progress).await;
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    progress(Install::Installing);
    match kind {
        Kind::Binary(path) | Kind::AppImage(path) => replace(&part, &path),
        Kind::Deb => {
            std::fs::rename(&part, &target).map_err(|e| e.to_string())?;
            let status = tokio::process::Command::new("pkexec")
                .args(["apt-get", "install", "-y"])
                .arg(&target)
                .status()
                .await
                .map_err(|e| format!("pkexec: {e}"))?;
            let _ = std::fs::remove_file(&target);
            match status.code() {
                Some(0) => Ok(()),
                Some(126) => Err("the password was not given".into()),
                _ => Err(format!("installing the package failed ({status})")),
            }
        }
    }
}

/// downloads `url` to `part`, checking its SHA-256
async fn download(
    url: &str,
    part: &Path,
    expected: &[u8],
    progress: &impl Fn(Install),
) -> Result<(), String> {
    let mut out = std::fs::File::create(part).map_err(|e| format!("{}: {e}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut told = 0u64;
    get(url, MAX_FILE, &mut |data, total| {
        out.write_all(data).map_err(|e| e.to_string())?;
        hasher.update(data);
        done += data.len() as u64;
        // about every percent
        if done - told >= (total / 100).max(256 * 1024) || done == total {
            told = done;
            progress(Install::Downloading { done, total });
        }
        Ok(())
    })
    .await?;
    out.sync_all().map_err(|e| e.to_string())?;
    if hasher.finalize().as_slice() != expected {
        return Err("the download does not match the signed checksum".into());
    }
    Ok(())
}

/// puts `new` in place of `path`, keeping the old one as `<path>.previous`
fn replace(new: &Path, path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    let mut previous = path.as_os_str().to_owned();
    previous.push(".previous");
    let previous = PathBuf::from(previous);
    let _ = std::fs::remove_file(&previous);
    if let Err(e) =
        std::fs::hard_link(path, &previous).or_else(|_| std::fs::copy(path, &previous).map(|_| ()))
    {
        log::warn!("the old version is not kept as {}: {e}", previous.display());
    }
    std::fs::rename(new, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn the_release_key_is_built_in() {
        assert_eq!(hex(RELEASE_KEY).map(|k| k.len()), Some(32));
    }

    #[test]
    fn a_signature_of_the_release_key() {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let sums = b"00  rustinput-linux-x86_64\n";
        let signature = pair.sign(sums);
        let key = pair.public_key().as_ref();
        assert!(signed(key, sums, signature.as_ref()));
        assert!(!signed(
            key,
            b"01  rustinput-linux-x86_64\n",
            signature.as_ref()
        ));
        assert!(!signed(
            &hex(RELEASE_KEY).unwrap(),
            sums,
            signature.as_ref()
        ));
    }

    #[test]
    fn the_line_of_a_file() {
        let sums = "ab01  rustinput-linux-x86_64\ncd02 *rustinput_0.51.0_amd64.deb\n";
        assert_eq!(
            checksum(sums, "rustinput-linux-x86_64"),
            Some(vec![0xab, 0x01])
        );
        assert_eq!(
            checksum(sums, "rustinput_0.51.0_amd64.deb"),
            Some(vec![0xcd, 0x02])
        );
        assert_eq!(checksum(sums, "rustinput-linux"), None);
    }

    #[test]
    fn addresses() {
        assert_eq!(
            url("https://github.com/a/b").unwrap(),
            Url {
                https: true,
                host: "github.com".into(),
                port: 443,
                path: "/a/b".into()
            }
        );
        assert_eq!(url("http://127.0.0.1:4380").unwrap().path, "/");
        assert!(url("ftp://x/y").is_err());
    }

    /// what `exchange` makes of `answer`: the body, or the redirect
    async fn answered(answer: &'static [u8]) -> Result<(Vec<u8>, Option<String>), String> {
        let target = url("http://example.test/a").unwrap();
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut request = vec![0; 1024];
            let _ = server.read(&mut request).await;
            let _ = server.write_all(answer).await;
        });
        let mut body = Vec::new();
        let answer = exchange(client, &target, 100, &mut |data, _| {
            body.extend_from_slice(data);
            Ok(())
        })
        .await?;
        Ok(match answer {
            Answer::Done => (body, None),
            Answer::Redirect(to) => (body, Some(to)),
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn answers_of_servers() {
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                        5\r\nhello\r\n6;x=y\r\n world\r\n0\r\n\r\n";
        assert_eq!(answered(chunked).await.unwrap().0, b"hello world");
        let sized = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(answered(sized).await.unwrap().0, b"hello");
        let moved = b"HTTP/1.1 302 Found\r\nLocation: https://cdn.test/f\r\n\r\n";
        assert_eq!(
            answered(moved).await.unwrap().1.as_deref(),
            Some("https://cdn.test/f")
        );
        // cut short, too big, missing
        assert!(
            answered(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nhello")
                .await
                .is_err()
        );
        assert!(
            answered(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n")
                .await
                .is_err()
        );
        assert!(answered(b"HTTP/1.1 404 Not Found\r\n\r\n").await.is_err());
    }

    #[test]
    fn the_files_of_each_kind() {
        if std::env::consts::ARCH != "x86_64" {
            return;
        }
        assert_eq!(
            Kind::Binary(PathBuf::new()).file("0.51.0").as_deref(),
            Some("rustinput-linux-x86_64")
        );
        assert_eq!(
            Kind::AppImage(PathBuf::new()).file("0.51.0").as_deref(),
            Some("rustinput_0.51.0_x86_64.AppImage")
        );
        assert_eq!(
            Kind::Deb.file("0.51.0").as_deref(),
            Some("rustinput_0.51.0_amd64.deb")
        );
    }
}
