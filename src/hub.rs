//! Connection hub: connected devices, chat broadcast, the attachment registry
//! and the auto-clear timer.
//!
//! The reference implementation uses a single goroutine that owns all state
//! and talks to it over channels. Here the state is a plain `Mutex` that is
//! never held across an `.await`. Sending to a client is a non-blocking
//! `try_send` into that client's bounded queue, so a slow device can never
//! stall the hub. If a queue is full, that client is dropped.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::SystemTime;

use axum::extract::ws::Message;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::Instant;

use crate::config::Config;
use crate::relay::{ActiveGuard, AttachResult, Mode, Pending};
use crate::timefmt::rfc3339;

pub type Shared = Arc<AppState>;

/// Outbound queue depth per client before it is considered stuck.
pub const CLIENT_QUEUE: usize = 256;
/// Maximum size of a chat text message (bytes).
pub const MAX_TEXT: usize = 512 * 1024;
const MAX_NAME_CHARS: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    #[default]
    File,
    Dir,
}

/// Attachment metadata as broadcast to clients. The bytes themselves stay on the owner's device.
#[derive(Debug, Clone, Serialize)]
pub struct FileMeta {
    pub id: String,
    pub name: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub mime: String,
    pub kind: FileKind,
    pub count: u64,
}

#[derive(Debug, Deserialize)]
struct ClientFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default, rename = "type")]
    mime: String,
    #[serde(default)]
    kind: FileKind,
    #[serde(default)]
    count: u64,
}

#[derive(Debug, Deserialize)]
struct ClientMsg {
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    file: Option<ClientFile>,
}

struct Client {
    ip: String,
    tx: mpsc::Sender<Message>,
    active: Arc<AtomicUsize>,
}

struct FileEntry {
    owner: u64,
    meta: FileMeta,
}

struct ClearState {
    interval_min: u32,
    paused: bool,
    deadline: Option<Instant>,
    wall: Option<SystemTime>,
    gen: u64,
}

struct Inner {
    next_conn: u64,
    next_id: u64,
    clients: HashMap<u64, Client>,
    files: HashMap<String, FileEntry>,
    gone: HashSet<String>,
    relays: HashMap<String, Pending>,
    clear: ClearState,
}

/// Why a transfer could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferError {
    NotFound,
    /// The owning device disconnected; its attachments are gone.
    Gone,
    /// The owning device already serves the maximum number of transfers.
    Busy,
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TransferError::NotFound => "file not found",
            TransferError::Gone => "the sender is offline; this attachment is no longer available",
            TransferError::Busy => "the sender is busy with other downloads; try again shortly",
        })
    }
}

/// A transfer request waiting for the owner to connect its relay socket.
pub struct TransferTicket {
    pub token: String,
    pub meta: FileMeta,
    pub attach: oneshot::Receiver<AttachResult>,
}

pub struct AppState {
    pub cfg: Config,
    inner: Mutex<Inner>,
    clear_notify: Arc<Notify>,
}

fn text(v: serde_json::Value) -> Message {
    Message::Text(v.to_string().into())
}

fn clean_name(name: &str, fallback: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || c == '/' || c == '\\' {
                '_'
            } else {
                c
            }
        })
        .take(MAX_NAME_CHARS)
        .collect();
    let s = s.trim();
    if s.is_empty() || s == "." || s == ".." {
        fallback.to_string()
    } else {
        s.to_string()
    }
}

fn clean_mime(m: &str) -> String {
    if m.len() <= 127 && m.bytes().all(|b| b.is_ascii_graphic()) {
        m.to_string()
    } else {
        String::new()
    }
}

impl AppState {
    /// Creates the state and starts the auto-clear timer task.
    pub fn start(cfg: Config) -> Shared {
        let now = Instant::now();
        let interval = cfg.auto_clear_min;
        let (deadline, wall) = if interval > 0 {
            let d = cfg.clear_unit * interval;
            (Some(now + d), Some(SystemTime::now() + d))
        } else {
            (None, None)
        };
        let st = Arc::new(AppState {
            cfg,
            inner: Mutex::new(Inner {
                next_conn: 1,
                next_id: 0,
                clients: HashMap::new(),
                files: HashMap::new(),
                gone: HashSet::new(),
                relays: HashMap::new(),
                clear: ClearState {
                    interval_min: interval,
                    paused: false,
                    deadline,
                    wall,
                    gen: 0,
                },
            }),
            clear_notify: Arc::new(Notify::new()),
        });
        tokio::spawn(run_clear_timer(
            Arc::downgrade(&st),
            st.clear_notify.clone(),
        ));
        st
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn broadcast_locked(inner: &mut Inner, msg: Message) {
        let mut stuck = Vec::new();
        for (id, c) in &inner.clients {
            if c.tx.try_send(msg.clone()).is_err() {
                stuck.push(*id);
            }
        }
        for id in stuck {
            if let Some(c) = inner.clients.remove(&id) {
                tracing::warn!("dropping unresponsive client {} ({})", id, c.ip);
            }
        }
    }

    fn device_count(inner: &Inner) -> usize {
        inner
            .clients
            .values()
            .map(|c| c.ip.as_str())
            .collect::<HashSet<_>>()
            .len()
    }

    fn config_msg(c: &ClearState) -> Message {
        text(json!({
            "type": "config",
            "config": {
                "intervalMin": c.interval_min,
                "paused": c.paused,
                "nextClearTime": c.wall.map(rfc3339),
            }
        }))
    }

    fn hello_msg(&self, ip: &str) -> Message {
        text(json!({
            "type": "hello",
            "version": self.cfg.version,
            "ip": ip,
            "transferCompression": self.cfg.transfer_compression,
            "browserDecodeMaxBytes": self.cfg.browser_decode_max_bytes,
            "maxChunk": lcf::MAX_CHUNK,
        }))
    }

    /// Registers a new WebSocket client and returns its connection id.
    pub fn register(&self, ip: String, tx: mpsc::Sender<Message>) -> u64 {
        let _ = tx.try_send(self.hello_msg(&ip));
        let mut inner = self.lock();
        let _ = tx.try_send(Self::config_msg(&inner.clear));
        let conn = inner.next_conn;
        inner.next_conn += 1;
        inner.clients.insert(
            conn,
            Client {
                ip: ip.clone(),
                tx,
                active: Arc::new(AtomicUsize::new(0)),
            },
        );
        let count = Self::device_count(&inner);
        tracing::info!("client connected: {ip} ({count} device(s))");
        Self::broadcast_locked(&mut inner, text(json!({"type": "clients", "count": count})));
        conn
    }

    /// Removes a client. Its attachments become unavailable to everyone.
    pub fn unregister(&self, conn: u64) {
        let mut inner = self.lock();
        let ip = inner
            .clients
            .remove(&conn)
            .map(|c| c.ip)
            .unwrap_or_default();
        let ids: Vec<String> = inner
            .files
            .iter()
            .filter(|(_, f)| f.owner == conn)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            inner.files.remove(id);
            inner.gone.insert(id.clone());
        }
        let tokens: Vec<String> = inner
            .relays
            .iter()
            .filter(|(_, p)| p.owner == conn)
            .map(|(t, _)| t.clone())
            .collect();
        let dropped: Vec<Pending> = tokens
            .iter()
            .filter_map(|t| inner.relays.remove(t))
            .collect();
        if !ids.is_empty() {
            Self::broadcast_locked(&mut inner, text(json!({"type": "unavailable", "ids": ids})));
        }
        let count = Self::device_count(&inner);
        Self::broadcast_locked(&mut inner, text(json!({"type": "clients", "count": count})));
        drop(inner);
        drop(dropped);
        tracing::info!("client disconnected: {ip} ({count} device(s))");
    }

    /// Handles a chat/attachment message from a client.
    pub fn handle_client_text(&self, conn: u64, raw: &str) {
        let Ok(msg) = serde_json::from_str::<ClientMsg>(raw) else {
            return;
        };
        if (msg.text.is_empty() && msg.file.is_none()) || msg.text.len() > MAX_TEXT {
            return;
        }
        let reference = msg.reference.filter(|r| r.len() <= 64);
        let mut inner = self.lock();
        let Some(ip) = inner.clients.get(&conn).map(|c| c.ip.clone()) else {
            return;
        };
        inner.next_id += 1;
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let id = format!("{millis:x}{:04x}", inner.next_id);
        let meta = msg.file.map(|f| {
            let kind = f.kind;
            FileMeta {
                id: id.clone(),
                name: clean_name(
                    &f.name,
                    if kind == FileKind::Dir {
                        "folder"
                    } else {
                        "file"
                    },
                ),
                size: f.size,
                mime: if kind == FileKind::Dir {
                    String::new()
                } else {
                    clean_mime(&f.mime)
                },
                kind,
                count: if kind == FileKind::Dir { f.count } else { 1 },
            }
        });
        if let Some(m) = &meta {
            inner.files.insert(
                id.clone(),
                FileEntry {
                    owner: conn,
                    meta: m.clone(),
                },
            );
        }
        let out = text(json!({
            "id": id,
            "ref": reference,
            "text": msg.text,
            "senderIp": ip,
            "file": meta,
        }));
        Self::broadcast_locked(&mut inner, out);
    }

    /// Returns metadata for a currently available attachment.
    pub fn file_meta(&self, id: &str) -> Result<FileMeta, TransferError> {
        let inner = self.lock();
        match inner.files.get(id) {
            Some(f) => Ok(f.meta.clone()),
            None if inner.gone.contains(id) => Err(TransferError::Gone),
            None => Err(TransferError::NotFound),
        }
    }

    /// Asks the owner of `id` to stream it. The returned ticket resolves once
    /// the owner has connected `/relay/{token}`.
    pub fn request_transfer(&self, id: &str, mode: Mode) -> Result<TransferTicket, TransferError> {
        let mut inner = self.lock();
        let (owner, meta) = match inner.files.get(id) {
            Some(f) => (f.owner, f.meta.clone()),
            None if inner.gone.contains(id) => return Err(TransferError::Gone),
            None => return Err(TransferError::NotFound),
        };
        let client = inner.clients.get(&owner).ok_or(TransferError::Gone)?;
        let guard = ActiveGuard::try_acquire(&client.active, self.cfg.max_transfers_per_sender)
            .ok_or(TransferError::Busy)?;
        let token = format!("{:032x}", rand::random::<u128>());
        let req = text(json!({
            "type": "fileRequest",
            "id": id,
            "token": token,
            "mode": mode.as_str(),
        }));
        if client.tx.try_send(req).is_err() {
            return Err(TransferError::Gone);
        }
        let (tx, rx) = oneshot::channel();
        inner.relays.insert(
            token.clone(),
            Pending {
                owner,
                meta: meta.clone(),
                mode,
                tx,
                guard,
            },
        );
        Ok(TransferTicket {
            token,
            meta,
            attach: rx,
        })
    }

    /// Claims a pending transfer (single use).
    pub fn claim(&self, token: &str) -> Option<Pending> {
        let mut inner = self.lock();
        let p = inner.relays.remove(token)?;
        if inner.clients.contains_key(&p.owner) {
            Some(p)
        } else {
            drop(inner);
            drop(p);
            None
        }
    }

    /// Abandons a pending transfer (receiver timed out or left).
    pub fn cancel(&self, token: &str) {
        let p = self.lock().relays.remove(token);
        drop(p);
    }

    fn reschedule_locked(&self, inner: &mut Inner) {
        let c = &mut inner.clear;
        c.gen += 1;
        if c.interval_min > 0 && !c.paused {
            let d = self.cfg.clear_unit * c.interval_min;
            c.deadline = Some(Instant::now() + d);
            c.wall = Some(SystemTime::now() + d);
        } else {
            c.deadline = None;
            c.wall = None;
        }
        self.clear_notify.notify_one();
    }

    fn clear_locked(inner: &mut Inner) {
        inner.files.clear();
        inner.gone.clear();
        Self::broadcast_locked(inner, text(json!({"type": "clear"})));
    }

    /// Clears all messages and attachments now and restarts the timer.
    pub fn clear_now(&self) {
        let mut inner = self.lock();
        Self::clear_locked(&mut inner);
        self.reschedule_locked(&mut inner);
        let cfg = Self::config_msg(&inner.clear);
        Self::broadcast_locked(&mut inner, cfg);
        tracing::info!("manual clear");
    }

    /// Sets the auto-clear interval in minutes (0 disables) and resumes the timer.
    pub fn set_interval(&self, minutes: u32) {
        let mut inner = self.lock();
        inner.clear.interval_min = minutes;
        inner.clear.paused = false;
        self.reschedule_locked(&mut inner);
        let cfg = Self::config_msg(&inner.clear);
        Self::broadcast_locked(&mut inner, cfg);
    }

    /// Pauses or resumes the auto-clear timer.
    pub fn toggle_pause(&self) {
        let mut inner = self.lock();
        if inner.clear.interval_min == 0 {
            return;
        }
        inner.clear.paused = !inner.clear.paused;
        self.reschedule_locked(&mut inner);
        let cfg = Self::config_msg(&inner.clear);
        Self::broadcast_locked(&mut inner, cfg);
    }

    fn clear_snapshot(&self) -> (Option<Instant>, u64) {
        let inner = self.lock();
        (inner.clear.deadline, inner.clear.gen)
    }

    fn auto_clear(&self, gen: u64) {
        let mut inner = self.lock();
        if inner.clear.gen != gen {
            return;
        }
        Self::clear_locked(&mut inner);
        self.reschedule_locked(&mut inner);
        let cfg = Self::config_msg(&inner.clear);
        Self::broadcast_locked(&mut inner, cfg);
        tracing::info!("auto-clear ({} min)", inner.clear.interval_min);
    }
}

async fn run_clear_timer(state: Weak<AppState>, notify: Arc<Notify>) {
    loop {
        let Some(st) = state.upgrade() else { return };
        let (deadline, gen) = st.clear_snapshot();
        drop(st);
        match deadline {
            Some(at) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(at) => {
                        match state.upgrade() {
                            Some(st) => st.auto_clear(gen),
                            None => return,
                        }
                    }
                    _ = notify.notified() => {}
                }
            }
            None => notify.notified().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> Config {
        Config {
            auto_clear_min: 1,
            clear_unit: Duration::from_millis(50),
            ..Config::default()
        }
    }

    fn drain(rx: &mut mpsc::Receiver<Message>) -> Vec<serde_json::Value> {
        let mut v = Vec::new();
        while let Ok(Message::Text(t)) = rx.try_recv() {
            v.push(serde_json::from_str(t.as_str()).unwrap());
        }
        v
    }

    #[tokio::test]
    async fn device_count_is_unique_ips_and_files_go_unavailable() {
        let st = AppState::start(cfg());
        let (tx1, mut rx1) = mpsc::channel(64);
        let (tx2, mut rx2) = mpsc::channel(64);
        let (tx3, _rx3) = mpsc::channel(64);
        let a = st.register("10.0.0.1".into(), tx1);
        let _b = st.register("10.0.0.1".into(), tx2);
        let c = st.register("10.0.0.2".into(), tx3);
        let msgs = drain(&mut rx1);
        assert_eq!(msgs[0]["type"], "hello");
        assert_eq!(msgs[1]["type"], "config");
        let counts: Vec<_> = msgs
            .iter()
            .filter(|m| m["type"] == "clients")
            .map(|m| m["count"].clone())
            .collect();
        assert_eq!(counts, vec![json!(1), json!(1), json!(2)]);

        st.handle_client_text(
            c,
            r#"{"ref":"r1","text":"hi","file":{"name":"../a.txt","size":3,"type":"text/plain"}}"#,
        );
        let m = drain(&mut rx2).pop().unwrap();
        assert_eq!(m["senderIp"], "10.0.0.2");
        assert_eq!(m["ref"], "r1");
        assert_eq!(m["file"]["name"], ".._a.txt");
        let id = m["id"].as_str().unwrap().to_string();
        assert!(st.file_meta(&id).is_ok());

        st.unregister(c);
        let msgs = drain(&mut rx1);
        assert!(msgs
            .iter()
            .any(|m| m["type"] == "unavailable" && m["ids"][0] == id.as_str()));
        assert_eq!(st.file_meta(&id).unwrap_err(), TransferError::Gone);
        assert_eq!(
            st.request_transfer(&id, Mode::Plain).err(),
            Some(TransferError::Gone)
        );
        st.unregister(a);
    }

    #[tokio::test]
    async fn transfer_limits_and_single_use_tokens() {
        let st = AppState::start(Config {
            max_transfers_per_sender: 1,
            ..cfg()
        });
        let (tx, mut rx) = mpsc::channel(64);
        let owner = st.register("10.0.0.3".into(), tx);
        st.handle_client_text(owner, r#"{"file":{"name":"x.bin","size":1}}"#);
        let id = drain(&mut rx).pop().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let t1 = st.request_transfer(&id, Mode::Plain).unwrap();
        let req = drain(&mut rx).pop().unwrap();
        assert_eq!(req["type"], "fileRequest");
        assert_eq!(req["token"], t1.token.as_str());
        assert_eq!(
            st.request_transfer(&id, Mode::Plain).err(),
            Some(TransferError::Busy)
        );
        let p = st.claim(&t1.token).unwrap();
        assert!(st.claim(&t1.token).is_none());
        drop(p);
        let t2 = st.request_transfer(&id, Mode::Passthrough).unwrap();
        st.cancel(&t2.token);
        assert!(st.claim(&t2.token).is_none());
        assert_eq!(
            st.request_transfer("nope", Mode::Plain).err(),
            Some(TransferError::NotFound)
        );
    }

    #[tokio::test]
    async fn auto_clear_fires_and_can_be_paused() {
        let st = AppState::start(cfg());
        let (tx, mut rx) = mpsc::channel(64);
        let c = st.register("10.0.0.4".into(), tx);
        st.handle_client_text(c, r#"{"file":{"name":"x","size":1}}"#);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let msgs = drain(&mut rx);
        assert!(msgs.iter().any(|m| m["type"] == "clear"));
        let id = msgs.iter().find(|m| m["file"].is_object()).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(st.file_meta(&id).unwrap_err(), TransferError::NotFound);

        st.toggle_pause();
        let cfgmsg = drain(&mut rx).pop().unwrap();
        assert_eq!(cfgmsg["config"]["paused"], true);
        assert!(cfgmsg["config"]["nextClearTime"].is_null());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(drain(&mut rx).iter().all(|m| m["type"] != "clear"));

        st.set_interval(0);
        let cfgmsg = drain(&mut rx).pop().unwrap();
        assert_eq!(cfgmsg["config"]["intervalMin"], 0);
        assert_eq!(cfgmsg["config"]["paused"], false);
        st.toggle_pause();
        assert!(drain(&mut rx).is_empty());

        st.clear_now();
        let msgs = drain(&mut rx);
        assert_eq!(msgs[0]["type"], "clear");
        assert_eq!(msgs[1]["type"], "config");
    }
}
