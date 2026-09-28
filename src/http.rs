//! HTTP routes.

use std::net::SocketAddr;

use axum::body::{Body, Bytes};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use qrcode::render::svg;
use qrcode::{EcLevel, QrCode};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::hub::{Shared, TransferError, CLIENT_QUEUE};
use crate::net_util::{content_disposition, real_ip};
use crate::relay::{self, Mode};

const INDEX_HTML: &str = include_str!("../web/index.html");
const STYLES_CSS: &str = include_str!("../web/styles.css");
const SCRIPT_JS: &str = include_str!("../web/script.js");
const WORKER_JS: &str = include_str!("../web/worker.js");
const LCF_WASM: &[u8] = include_bytes!("../web/lcf.wasm");

/// Largest control message accepted on the main socket.
const MAX_WS_MESSAGE: usize = 1 << 20;

pub fn router(state: Shared) -> Router {
    Router::new()
        .route(
            "/",
            get(|| asset("text/html; charset=utf-8", INDEX_HTML.as_bytes())),
        )
        .route(
            "/styles.css",
            get(|| asset("text/css; charset=utf-8", STYLES_CSS.as_bytes())),
        )
        .route(
            "/script.js",
            get(|| asset("text/javascript; charset=utf-8", SCRIPT_JS.as_bytes())),
        )
        .route(
            "/worker.js",
            get(|| asset("text/javascript; charset=utf-8", WORKER_JS.as_bytes())),
        )
        .route("/lcf.wasm", get(|| asset("application/wasm", LCF_WASM)))
        .route("/api/version", get(version))
        .route("/qr", get(qr))
        .route("/ws", get(ws_handler))
        .route("/file/{id}", get(download))
        .route("/pull/{id}", get(pull))
        .route("/relay/{token}", get(relay_handler))
        .route("/clear", post(clear))
        .route("/set-interval", post(set_interval))
        .route("/toggle-pause", post(toggle_pause))
        .fallback(|| async { (StatusCode::NOT_FOUND, "404 page not found") })
        .with_state(state)
}

fn no_cache(h: &mut HeaderMap) {
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    h.insert(header::EXPIRES, HeaderValue::from_static("0"));
}

async fn asset(content_type: &'static str, body: &'static [u8]) -> Response {
    let mut res = Response::new(Body::from(body));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    no_cache(h);
    res
}

async fn version(State(st): State<Shared>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain")],
        st.cfg.version.clone(),
    )
        .into_response()
}

async fn qr(State(st): State<Shared>) -> Response {
    let Some(host) = st.cfg.advertised_host.clone() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to determine local IP",
        )
            .into_response();
    };
    let url = format!("http://{host}:{}", st.cfg.port);
    match QrCode::with_error_correction_level(url.as_bytes(), EcLevel::M) {
        Ok(code) => {
            let svg = code
                .render::<svg::Color>()
                .min_dimensions(256, 256)
                .quiet_zone(true)
                .build();
            let mut res = Response::new(Body::from(svg));
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/svg+xml"),
            );
            no_cache(res.headers_mut());
            res
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Error generating QR code",
        )
            .into_response(),
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(st): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = real_ip(&headers, peer);
    ws.max_message_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| client_session(st, socket, ip))
}

async fn client_session(st: Shared, socket: WebSocket, ip: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Message>(CLIENT_QUEUE);
    let conn = st.register(ip, tx);
    let writer = async {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    };
    let reader = async {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Text(t) => st.handle_client_text(conn, t.as_str()),
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    tokio::select! {
        _ = writer => {}
        _ = reader => {}
    }
    st.unregister(conn);
}

fn transfer_error(e: TransferError) -> Response {
    let code = match e {
        TransferError::NotFound => StatusCode::NOT_FOUND,
        TransferError::Gone => StatusCode::GONE,
        TransferError::Busy => StatusCode::TOO_MANY_REQUESTS,
    };
    (code, e.to_string()).into_response()
}

/// `GET /file/{id}`: asks the owner to stream the attachment and relays it as
/// a normal download (decoded by the server; folders arrive as a ZIP).
async fn download(State(st): State<Shared>, Path(id): Path<String>) -> Response {
    let ticket = match st.request_transfer(&id, Mode::Plain) {
        Ok(t) => t,
        Err(e) => return transfer_error(e),
    };
    let attach = match tokio::time::timeout(st.cfg.transfer_timeout, ticket.attach).await {
        Ok(Ok(Ok(a))) => a,
        Ok(Ok(Err(msg))) => return (StatusCode::BAD_GATEWAY, msg).into_response(),
        Ok(Err(_)) => return transfer_error(TransferError::Gone),
        Err(_) => {
            st.cancel(&ticket.token);
            return (
                StatusCode::GATEWAY_TIMEOUT,
                "the sender did not respond; is its browser tab still open?",
            )
                .into_response();
        }
    };
    let mut res = Response::new(Body::from_stream(ReceiverStream::new(attach.rx)));
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&attach.content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(attach.len));
    if let Ok(v) = HeaderValue::from_str(&content_disposition(&attach.filename)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// `WS /pull/{id}`: relays LCF1 frames still compressed; the receiving browser
/// decodes them with the WASM codec.
async fn pull(ws: WebSocketUpgrade, State(st): State<Shared>, Path(id): Path<String>) -> Response {
    if let Err(e) = st.file_meta(&id) {
        return transfer_error(e);
    }
    ws.max_message_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| pull_session(st, id, socket))
}

async fn pull_session(st: Shared, id: String, mut socket: WebSocket) {
    let result = pull_forward(&st, &id, &mut socket).await;
    let close = match result {
        Ok(()) => CloseFrame {
            code: 1000,
            reason: "done".into(),
        },
        Err(e) => {
            let _ = socket
                .send(Message::Binary(
                    lcf::encode_control(lcf::Kind::Error, e.as_bytes()).into(),
                ))
                .await;
            CloseFrame {
                code: 1011,
                reason: "transfer failed".into(),
            }
        }
    };
    let _ = socket.send(Message::Close(Some(close))).await;
}

async fn pull_forward(st: &Shared, id: &str, socket: &mut WebSocket) -> Result<(), String> {
    let ticket = st
        .request_transfer(id, Mode::Passthrough)
        .map_err(|e| e.to_string())?;
    let mut attach = match tokio::time::timeout(st.cfg.transfer_timeout, ticket.attach).await {
        Ok(Ok(Ok(a))) => a,
        Ok(Ok(Err(msg))) => return Err(msg),
        Ok(Err(_)) => return Err(TransferError::Gone.to_string()),
        Err(_) => {
            st.cancel(&ticket.token);
            return Err("the sender did not respond".into());
        }
    };
    let mut saw_end = false;
    while let Some(item) = attach.rx.recv().await {
        let frame = item.map_err(|e| e.to_string())?;
        saw_end = frame.first() == Some(&(lcf::Kind::End as u8));
        socket
            .send(Message::Binary(frame))
            .await
            .map_err(|_| "receiver left".to_string())?;
    }
    if saw_end {
        Ok(())
    } else {
        Err("transfer aborted by the sender".into())
    }
}

/// `WS /relay/{token}`: the sending browser streams the requested attachment here.
async fn relay_handler(
    ws: WebSocketUpgrade,
    State(st): State<Shared>,
    Path(token): Path<String>,
) -> Response {
    let Some(pending) = st.claim(&token) else {
        return (StatusCode::NOT_FOUND, "unknown or expired transfer token").into_response();
    };
    let max = lcf::HEADER_LEN + lcf::MAX_CONTROL;
    ws.max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| relay::run(pending, socket))
}

async fn clear(State(st): State<Shared>) -> StatusCode {
    st.clear_now();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct IntervalReq {
    interval: i64,
}

async fn set_interval(State(st): State<Shared>, body: Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<IntervalReq>(&body) else {
        return (StatusCode::BAD_REQUEST, "Invalid request").into_response();
    };
    if req.interval < 0 || req.interval > u32::MAX as i64 {
        return (StatusCode::BAD_REQUEST, "Interval must be >= 0").into_response();
    }
    st.set_interval(req.interval as u32);
    StatusCode::NO_CONTENT.into_response()
}

async fn toggle_pause(State(st): State<Shared>) -> StatusCode {
    st.toggle_pause();
    StatusCode::NO_CONTENT
}
