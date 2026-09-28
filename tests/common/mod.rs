#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use local_clipboard::{AppState, Config};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub async fn spawn(cfg: Config) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::start(cfg);
    tokio::spawn(local_clipboard::serve(listener, state));
    addr
}

pub fn test_config() -> Config {
    Config {
        advertised_host: Some("192.168.1.10".into()),
        transfer_timeout: Duration::from_secs(5),
        ..Config::default()
    }
}

pub async fn connect(addr: SocketAddr, path: &str, forwarded_for: Option<&str>) -> Ws {
    let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
    if let Some(ip) = forwarded_for {
        req.headers_mut()
            .insert("x-forwarded-for", ip.parse().unwrap());
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws
}

/// Reads JSON text messages until one satisfies `pred`.
pub async fn wait_for(ws: &mut Ws, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    if pred(&v) {
                        return v;
                    }
                }
                Some(Ok(_)) => continue,
                other => panic!("socket ended while waiting: {other:?}"),
            }
        }
    })
    .await
    .expect("timed out waiting for message")
}

pub async fn send_json(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// Announces an attachment and returns its server-assigned id.
pub async fn share(ws: &mut Ws, file: Value) -> String {
    let r = format!("ref-{}", next_ref());
    send_json(ws, serde_json::json!({"ref": r, "text": "", "file": file})).await;
    let echo = wait_for(ws, |m| m["ref"] == r.as_str()).await;
    echo["id"].as_str().unwrap().to_string()
}

fn next_ref() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// Waits for the server to ask this client for `id`; returns `(token, mode)`.
pub async fn wait_request(ws: &mut Ws, id: &str) -> (String, String) {
    let m = wait_for(ws, |m| m["type"] == "fileRequest" && m["id"] == id).await;
    (
        m["token"].as_str().unwrap().to_string(),
        m["mode"].as_str().unwrap().to_string(),
    )
}

/// Streams `data` over a relay socket in `chunk`-sized LCF1 frames, mixing
/// compressed and raw frames to exercise both paths.
pub async fn send_frames(relay: &mut Ws, data: &[u8], chunk: usize) {
    for (i, c) in data.chunks(chunk.max(1)).enumerate() {
        let f = lcf::encode_data(c, i % 3 != 2).unwrap();
        relay.send(Message::Binary(f.into())).await.unwrap();
    }
}

pub async fn send_end(relay: &mut Ws) {
    relay
        .send(Message::Binary(
            lcf::encode_control(lcf::Kind::End, &[]).into(),
        ))
        .await
        .unwrap();
}

pub fn text_payload(n: usize) -> Vec<u8> {
    let words = b"local clipboard shares text files and folders across the lan ";
    (0..n).map(|i| words[i % words.len()]).collect()
}

pub fn random_payload(n: usize, mut seed: u64) -> Vec<u8> {
    (0..n)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        })
        .collect()
}
