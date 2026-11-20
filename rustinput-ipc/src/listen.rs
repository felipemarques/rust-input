use futures::{Stream, StreamExt, stream::SelectAll};
#[cfg(unix)]
use std::path::PathBuf;
use std::{
    io::ErrorKind,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_stream::wrappers::LinesStream;

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::TcpListener;
#[cfg(windows)]
use tokio::net::TcpStream;

use crate::{FrontendEvent, FrontendRequest, IpcError, IpcListenerCreationError};

pub struct AsyncFrontendListener {
    #[cfg(windows)]
    listener: TcpListener,
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    #[cfg(unix)]
    line_streams: SelectAll<LinesStream<BufReader<ReadHalf<UnixStream>>>>,
    #[cfg(windows)]
    line_streams: SelectAll<LinesStream<BufReader<ReadHalf<TcpStream>>>>,
    /// the queues of the events to the frontends, see [`spawn_writer`]
    tx_streams: Vec<mpsc::Sender<Arc<str>>>,
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
            (socket_path, listener)
        };

        #[cfg(windows)]
        let listener = match TcpListener::bind("127.0.0.1:5252").await {
            Ok(ls) => ls,
            // some other rustinput instance has bound the socket in the meantime
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                return Err(IpcListenerCreationError::AlreadyRunning);
            }
            Err(e) => return Err(IpcListenerCreationError::Bind(e)),
        };

        let adapter = Self {
            listener,
            #[cfg(unix)]
            socket_path,
            line_streams: SelectAll::new(),
            tx_streams: vec![],
        };

        Ok(adapter)
    }

    /// Queues `notify` for every frontend, without waiting for any of them.
    pub async fn broadcast(&mut self, notify: FrontendEvent) {
        let mut json = serde_json::to_string(&notify).unwrap();
        json.push('\n');
        let json: Arc<str> = json.into();
        self.tx_streams
            .retain(|tx| match tx.try_send(json.clone()) {
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
        if let Poll::Ready(Some(Ok(l))) = self.line_streams.poll_next_unpin(cx) {
            let request = serde_json::from_str(l.as_str()).map_err(|e| e.into());
            return Poll::Ready(Some(request));
        }
        let mut sync = false;
        while let Poll::Ready(Ok((stream, _))) = self.listener.poll_accept(cx) {
            let (rx, tx) = tokio::io::split(stream);
            let buf_reader = BufReader::new(rx);
            let lines = buf_reader.lines();
            let lines = LinesStream::new(lines);
            self.line_streams.push(lines);
            self.tx_streams.push(spawn_writer(tx));
            sync = true;
        }
        if sync {
            Poll::Ready(Some(Ok(FrontendRequest::Sync)))
        } else {
            Poll::Pending
        }
    }
}
