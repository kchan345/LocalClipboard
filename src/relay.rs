//! Streams one attachment from the sending browser to a receiver.
//!
//! The sender connects `WS /relay/{token}` and sends LCF1 frames (see the `lcf`
//! crate). Depending on how the receiver asked for the file:
//!
//! * [`Mode::Plain`] (`GET /file/{id}`): the server decodes each lz4 chunk and
//!   streams plain bytes (or, for folders, a generated ZIP) as the HTTP body.
//! * [`Mode::Passthrough`] (`WS /pull/{id}`): frames are validated and forwarded
//!   still compressed; the receiving browser decodes them with the WASM codec.
//!
//! Backpressure: the bounded `data` channel sits between the sender socket and
//! the receiver. When it is full the relay stops reading the sender socket, TCP
//! flow control pushes back to the browser, and its `bufferedAmount` grows,
//! which pauses reading more of the file from disk.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use bytes::Bytes;
use lcf::Kind;
use tokio::sync::{mpsc, oneshot};

use crate::hub::{FileKind, FileMeta};
use crate::zip::{ManifestEntry, Plan, ZipStream};

/// Chunks buffered between sender and receiver per transfer.
pub const RELAY_QUEUE: usize = 4;
/// A sender that goes quiet for this long mid-transfer is considered gone.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plain,
    Passthrough,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Plain => "plain",
            Mode::Passthrough => "passthrough",
        }
    }
}

/// What the receiver side gets once the sender is connected.
pub struct Attach {
    /// Exact number of bytes the receiver will get (HTTP `Content-Length`).
    pub len: u64,
    pub filename: String,
    pub content_type: String,
    pub rx: mpsc::Receiver<io::Result<Bytes>>,
}

/// `Err` carries a message explaining why the sender's stream was rejected.
pub type AttachResult = Result<Attach, String>;

/// Counts a transfer against the sender's concurrency limit until dropped.
pub struct ActiveGuard(Arc<AtomicUsize>);

impl ActiveGuard {
    pub fn try_acquire(counter: &Arc<AtomicUsize>, max: usize) -> Option<ActiveGuard> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < max).then_some(n + 1))
            .ok()
            .map(|_| ActiveGuard(counter.clone()))
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A transfer the owner has been asked to start.
pub struct Pending {
    pub owner: u64,
    pub meta: FileMeta,
    pub mode: Mode,
    pub tx: oneshot::Sender<AttachResult>,
    pub guard: ActiveGuard,
}

type DataTx = mpsc::Sender<io::Result<Bytes>>;

async fn next_frame(socket: &mut WebSocket) -> Result<Bytes, String> {
    loop {
        let msg = tokio::time::timeout(IDLE_TIMEOUT, socket.recv())
            .await
            .map_err(|_| "sender stalled".to_string())?;
        match msg {
            Some(Ok(Message::Binary(b))) => return Ok(b),
            Some(Ok(Message::Close(_))) | None => return Err("sender disconnected".into()),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("sender socket error: {e}")),
        }
    }
}

async fn send(tx: &DataTx, b: Bytes) -> Result<(), String> {
    tx.send(Ok(b))
        .await
        .map_err(|_| "receiver cancelled the download".to_string())
}

fn sender_error(payload: &[u8]) -> String {
    format!("sender aborted: {}", String::from_utf8_lossy(payload))
}

fn content_type(meta: &FileMeta) -> String {
    if meta.mime.is_empty() {
        "application/octet-stream".into()
    } else {
        meta.mime.clone()
    }
}

/// Runs a claimed transfer on the sender's relay socket.
pub async fn run(p: Pending, mut socket: WebSocket) {
    let Pending {
        meta,
        mode,
        tx,
        guard,
        ..
    } = p;
    let _guard = guard;
    let (data_tx, data_rx) = mpsc::channel(RELAY_QUEUE);
    let res = pump(&mut socket, &meta, mode, tx, data_rx, &data_tx).await;
    let close = match &res {
        Ok(()) => CloseFrame {
            code: 1000,
            reason: "done".into(),
        },
        Err(e) => {
            tracing::warn!("transfer of {:?} failed: {e}", meta.name);
            let _ = data_tx.try_send(Err(io::Error::other(e.clone())));
            let mut reason = e.clone();
            reason.truncate(120);
            while !reason.is_char_boundary(reason.len()) {
                reason.pop();
            }
            CloseFrame {
                code: 1011,
                reason: reason.into(),
            }
        }
    };
    let _ = socket.send(Message::Close(Some(close))).await;
}

async fn pump(
    socket: &mut WebSocket,
    meta: &FileMeta,
    mode: Mode,
    attach: oneshot::Sender<AttachResult>,
    rx: mpsc::Receiver<io::Result<Bytes>>,
    data: &DataTx,
) -> Result<(), String> {
    match (mode, meta.kind) {
        (Mode::Passthrough, FileKind::Dir) => {
            let e = "folders are delivered as zip downloads".to_string();
            let _ = attach.send(Err(e.clone()));
            Err(e)
        }
        (Mode::Passthrough, FileKind::File) => {
            attach
                .send(Ok(Attach {
                    len: meta.size,
                    filename: meta.name.clone(),
                    content_type: content_type(meta),
                    rx,
                }))
                .map_err(|_| "receiver left".to_string())?;
            let mut total = 0u64;
            loop {
                let buf = next_frame(socket).await?;
                let f = lcf::parse(&buf).map_err(|e| e.to_string())?;
                match f.kind {
                    Kind::Raw | Kind::Lz4 => {
                        total += f.raw_len as u64;
                        if total > meta.size {
                            return Err("sender sent more data than announced".into());
                        }
                        send(data, buf).await?;
                    }
                    Kind::End => {
                        if total != meta.size {
                            return Err(format!("sender sent {total} of {} bytes", meta.size));
                        }
                        send(data, buf).await?;
                        return Ok(());
                    }
                    Kind::Error => return Err(sender_error(f.payload)),
                    Kind::Manifest => return Err("unexpected manifest".into()),
                }
            }
        }
        (Mode::Plain, FileKind::File) => {
            attach
                .send(Ok(Attach {
                    len: meta.size,
                    filename: meta.name.clone(),
                    content_type: content_type(meta),
                    rx,
                }))
                .map_err(|_| "receiver left".to_string())?;
            let mut total = 0u64;
            loop {
                let buf = next_frame(socket).await?;
                let f = lcf::parse(&buf).map_err(|e| e.to_string())?;
                match f.kind {
                    Kind::Raw | Kind::Lz4 => {
                        total += f.raw_len as u64;
                        if total > meta.size {
                            return Err("sender sent more data than announced".into());
                        }
                        let plain = if f.kind == Kind::Raw {
                            buf.slice(lcf::HEADER_LEN..)
                        } else {
                            Bytes::from(lcf::decode(&f).map_err(|e| e.to_string())?)
                        };
                        if !plain.is_empty() {
                            send(data, plain).await?;
                        }
                    }
                    Kind::End => {
                        if total != meta.size {
                            return Err(format!("sender sent {total} of {} bytes", meta.size));
                        }
                        return Ok(());
                    }
                    Kind::Error => return Err(sender_error(f.payload)),
                    Kind::Manifest => return Err("unexpected manifest".into()),
                }
            }
        }
        (Mode::Plain, FileKind::Dir) => {
            let plan = match read_manifest(socket).await {
                Ok(p) => p,
                Err(e) => {
                    let _ = attach.send(Err(e.clone()));
                    return Err(e);
                }
            };
            attach
                .send(Ok(Attach {
                    len: plan.total_len(),
                    filename: format!("{}.zip", meta.name),
                    content_type: "application/zip".into(),
                    rx,
                }))
                .map_err(|_| "receiver left".to_string())?;
            let mut zip = ZipStream::new(plan);
            for b in zip.begin() {
                send(data, b).await?;
            }
            loop {
                let buf = next_frame(socket).await?;
                let f = lcf::parse(&buf).map_err(|e| e.to_string())?;
                match f.kind {
                    Kind::Raw | Kind::Lz4 => {
                        let plain = if f.kind == Kind::Raw {
                            buf.slice(lcf::HEADER_LEN..)
                        } else {
                            Bytes::from(lcf::decode(&f).map_err(|e| e.to_string())?)
                        };
                        for b in zip.feed(plain).map_err(|e| e.to_string())? {
                            send(data, b).await?;
                        }
                    }
                    Kind::End => {
                        let tail = zip.finish().map_err(|e| e.to_string())?;
                        send(data, tail).await?;
                        return Ok(());
                    }
                    Kind::Error => return Err(sender_error(f.payload)),
                    Kind::Manifest => return Err("duplicate manifest".into()),
                }
            }
        }
    }
}

async fn read_manifest(socket: &mut WebSocket) -> Result<Plan, String> {
    let buf = next_frame(socket).await?;
    let f = lcf::parse(&buf).map_err(|e| e.to_string())?;
    match f.kind {
        Kind::Manifest => {}
        Kind::Error => return Err(sender_error(f.payload)),
        _ => return Err("expected a folder manifest".into()),
    }
    let entries: Vec<ManifestEntry> =
        serde_json::from_slice(f.payload).map_err(|e| format!("invalid manifest: {e}"))?;
    Plan::new(&entries).map_err(|e| e.to_string())
}
