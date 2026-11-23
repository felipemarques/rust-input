use crate::{ConnectionError, FrontendEvent, FrontendRequest, IpcError};
use std::{cmp::min, io, thread, time::Duration};

#[cfg(unix)]
use std::{
    io::{BufReader, LineWriter, Lines, prelude::*},
    os::unix::net::UnixStream,
};

pub struct FrontendEventReader {
    #[cfg(unix)]
    lines: Lines<BufReader<UnixStream>>,
    /// the lines the thread of the pipe read, see [`windows::open`]
    #[cfg(windows)]
    lines: std::sync::mpsc::Receiver<io::Result<String>>,
}

pub struct FrontendRequestWriter {
    #[cfg(unix)]
    line_writer: LineWriter<UnixStream>,
    /// the lines for the thread of the pipe to write
    #[cfg(windows)]
    lines: tokio::sync::mpsc::UnboundedSender<String>,
}

impl FrontendEventReader {
    pub fn next_event(&mut self) -> Option<Result<FrontendEvent, IpcError>> {
        #[cfg(unix)]
        let line = self.lines.next()?;
        #[cfg(windows)]
        let line = self.lines.recv().ok()?;
        match line {
            Err(e) => Some(Err(e.into())),
            Ok(l) => Some(serde_json::from_str(l.as_str()).map_err(|e| e.into())),
        }
    }
}

impl FrontendRequestWriter {
    pub fn request(&mut self, request: FrontendRequest) -> Result<(), io::Error> {
        let mut json = serde_json::to_string(&request).unwrap();
        log::debug!("requesting: {json}");
        json.push('\n');
        #[cfg(unix)]
        self.line_writer.write_all(json.as_bytes())?;
        #[cfg(windows)]
        self.lines.send(json).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the daemon closed the connection",
            )
        })?;
        Ok(())
    }
}

/// Try to connect to the service once without retrying.
pub fn try_connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    #[cfg(unix)]
    {
        let socket_path = crate::default_socket_path()?;
        make_connection(UnixStream::connect(&socket_path)?)
    }
    #[cfg(windows)]
    {
        windows::open()
    }
}

pub fn connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    #[cfg(unix)]
    {
        make_connection(wait_for_service()?)
    }
    #[cfg(windows)]
    {
        let mut duration = Duration::from_millis(10);
        loop {
            match windows::open() {
                Ok(connection) => break Ok(connection),
                // a pipe of someone else: waiting does not help
                Err(ConnectionError::Io(e)) if e.kind() == io::ErrorKind::PermissionDenied => {
                    break Err(e.into());
                }
                Err(_) => thread::sleep(exponential_back_off(&mut duration)),
            }
        }
    }
}

#[cfg(unix)]
fn make_connection(
    rx: UnixStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { lines };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

/// The named pipe of the session, which reads and writes only overlapped
/// without blocking one direction on the other: a thread of its own with a
/// Tokio runtime does both, and hands the lines over in channels.
#[cfg(windows)]
mod windows {
    use std::{io, sync::mpsc as std_mpsc, thread};

    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        sync::mpsc,
    };

    use super::{FrontendEventReader, FrontendRequestWriter};
    use crate::ConnectionError;

    pub(super) fn open() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
        let (opened_tx, opened) = std_mpsc::channel::<io::Result<()>>();
        let (events_tx, events) = std_mpsc::channel();
        let (requests, mut requests_rx) = mpsc::unbounded_channel::<String>();
        thread::Builder::new()
            .name("rustinput-ipc".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => return drop(opened_tx.send(Err(e))),
                };
                runtime.block_on(async move {
                    let pipe = match crate::windows_pipe::open() {
                        Ok(pipe) => pipe,
                        Err(e) => return drop(opened_tx.send(Err(e))),
                    };
                    let _ = opened_tx.send(Ok(()));
                    let (rx, mut tx) = tokio::io::split(pipe);
                    let read = async move {
                        let mut lines = BufReader::new(rx).lines();
                        loop {
                            match lines.next_line().await {
                                Ok(Some(line)) => {
                                    if events_tx.send(Ok(line)).is_err() {
                                        return;
                                    }
                                }
                                Ok(None) => return,
                                Err(e) => return drop(events_tx.send(Err(e))),
                            }
                        }
                    };
                    let write = async move {
                        while let Some(line) = requests_rx.recv().await {
                            if tx.write_all(line.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    };
                    // the window stops reading or writing only when it quits
                    tokio::join!(read, write);
                });
            })?;
        match opened.recv() {
            Ok(Ok(())) => Ok((
                FrontendEventReader { lines: events },
                FrontendRequestWriter { lines: requests },
            )),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(io::Error::other("the thread of the pipe ended").into()),
        }
    }
}

/// wait for the rustinput socket to come online
#[cfg(unix)]
fn wait_for_service() -> Result<UnixStream, ConnectionError> {
    let socket_path = crate::default_socket_path()?;
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = UnixStream::connect(&socket_path) {
            break Ok(stream);
        }
        // a signaling mechanism or inotify could be used to
        // improve this
        thread::sleep(exponential_back_off(&mut duration));
    }
}

fn exponential_back_off(duration: &mut Duration) -> Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    *duration
}
