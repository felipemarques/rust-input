use futures::{Stream, StreamExt, future, stream::BoxStream, stream::SelectAll};
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_stream::wrappers::LinesStream;

#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;

use crate::{FrontendEvent, FrontendRequest, IpcError, IpcListenerCreationError};

/// what one frontend asked, tagged with its connection
type Requests = BoxStream<'static, (u64, Result<FrontendRequest, IpcError>)>;

pub struct AsyncFrontendListener {
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    /// the windows accepted by [`crate::windows_pipe::listen`]
    #[cfg(windows)]
    incoming: mpsc::UnboundedReceiver<NamedPipeServer>,
    requests: SelectAll<Requests>,
    /// the queues of the events to the frontends, see [`spawn_writer`]
    tx_streams: Vec<(u64, mpsc::Sender<Arc<str>>)>,
    next_id: u64,
}

/// Events waiting for a frontend that does not read them, a frozen one: past
/// this it is disconnected. Some 17 minutes of the once a second
/// [`FrontendEvent::LinkStats`].
const FRONTEND_QUEUE: usize = 1024;

/// Writes the events queued for one frontend on a task of its own, so a
/// frontend that does not read them never holds up the service.
fn spawn_writer<W>(mut tx: W) -> mpsc::Sender<Arc<str>>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (sender, mut queue) = mpsc::channel::<Arc<str>>(FRONTEND_QUEUE);
    tokio::spawn(async move {
        while let Some(json) = queue.recv().await {
            if tx.write_all(json.as_bytes()).await.is_err() {
                return;
            }
        }
        // dropped by the service: the frontend reads the end of the stream
        let _ = tx.shutdown().await;
    });
    sender
}

/// The requests of one frontend, ending after the first that is not one:
/// whatever sent it is no frontend of RustInput, and is disconnected.
fn requests<R>(id: u64, rx: R) -> Requests
where
    R: AsyncRead + Send + 'static,
{
    LinesStream::new(BufReader::new(rx).lines())
        .scan(true, |going, line| {
            if !*going {
                return future::ready(None);
            }
            let request = match line {
                Ok(line) => serde_json::from_str(&line).map_err(IpcError::from),
                // gone: the end of its stream
                Err(e) => {
                    log::debug!("a frontend went away: {e}");
                    return future::ready(None);
                }
            };
            *going = request.is_ok();
            future::ready(Some(request))
        })
        .map(move |request| (id, request))
        .boxed()
}

impl AsyncFrontendListener {
    pub async fn new() -> Result<Self, IpcListenerCreationError> {
        #[cfg(unix)]
        let (socket_path, listener) = {
            let socket_path = crate::default_socket_path()?;

            log::debug!("remove socket: {socket_path:?}");
            if socket_path.exists() {
                // try to connect to see if some other instance
                // of rustinput is already running
                match UnixStream::connect(&socket_path).await {
                    // connected -> rustinput is already running
                    Ok(_) => return Err(IpcListenerCreationError::AlreadyRunning),
                    // rustinput is not running but a socket was left behind
                    Err(e) => {
                        log::debug!("{socket_path:?}: {e} - removing left behind socket");
                        let _ = std::fs::remove_file(&socket_path);
                    }
                }
            }
            let listener = match UnixListener::bind(&socket_path) {
                Ok(ls) => ls,
                // some other rustinput instance has bound the socket in the meantime
                Err(e) if e.kind() == ErrorKind::AddrInUse => {
                    return Err(IpcListenerCreationError::AlreadyRunning);
                }
                Err(e) => return Err(IpcListenerCreationError::Bind(e)),
            };
            // only this user, whatever the umask
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) =
                    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
                {
                    log::warn!("{socket_path:?}: could not make it private: {e}");
                }
            }
            (socket_path, listener)
        };

        #[cfg(windows)]
        let incoming = match crate::windows_pipe::listen() {
            Ok(incoming) => incoming,
            // the pipe of this session exists: another daemon runs here
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(IpcListenerCreationError::AlreadyRunning);
            }
            Err(e) => return Err(IpcListenerCreationError::Bind(e)),
        };

        Ok(Self {
            #[cfg(unix)]
            listener,
            #[cfg(unix)]
            socket_path,
            #[cfg(windows)]
            incoming,
            requests: SelectAll::new(),
            tx_streams: vec![],
            next_id: 0,
        })
    }

    /// Queues `notify` for every frontend, without waiting for any of them.
    pub async fn broadcast(&mut self, notify: FrontendEvent) {
        let mut json = serde_json::to_string(&notify).unwrap();
        json.push('\n');
        let json: Arc<str> = json.into();
        self.tx_streams
            .retain(|(_, tx)| match tx.try_send(json.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    log::warn!("a frontend does not read its events, disconnecting it");
                    false
                }
                Err(TrySendError::Closed(_)) => false,
            });
    }

    /// whether a frontend is connected
    pub fn has_frontends(&self) -> bool {
        !self.tx_streams.is_empty()
    }

    fn add<S>(&mut self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let id = self.next_id;
        self.next_id += 1;
        let (rx, tx) = tokio::io::split(stream);
        self.requests.push(requests(id, rx));
        self.tx_streams.push((id, spawn_writer(tx)));
    }
}

/// the user this process runs as
#[cfg(unix)]
fn own_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(unix)]
impl Drop for AsyncFrontendListener {
    fn drop(&mut self) {
        log::debug!("remove socket: {:?}", self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Stream for AsyncFrontendListener {
    type Item = Result<FrontendRequest, IpcError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Some((id, request))) = self.requests.poll_next_unpin(cx) {
            if request.is_err() {
                // its stream ends with this; no more events for it either
                self.tx_streams.retain(|(other, _)| *other != id);
            }
            return Poll::Ready(Some(request));
        }
        let mut sync = false;
        #[cfg(unix)]
        while let Poll::Ready(Ok((stream, _))) = self.listener.poll_accept(cx) {
            match stream.peer_cred() {
                Ok(peer) if peer.uid() == own_uid() => {
                    self.add(stream);
                    sync = true;
                }
                Ok(peer) => log::warn!("refused a connection of the user {}", peer.uid()),
                Err(e) => log::warn!("refused a connection of an unknown user: {e}"),
            }
        }
        #[cfg(windows)]
        while let Poll::Ready(Some(pipe)) = self.incoming.poll_recv(cx) {
            self.add(pipe);
            sync = true;
        }
        if sync {
            Poll::Ready(Some(Ok(FrontendRequest::Sync)))
        } else {
            Poll::Pending
        }
    }
}
