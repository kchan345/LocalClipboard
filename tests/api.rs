mod common;

use common::*;
use serde_json::json;

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

#[tokio::test]
async fn static_assets_and_version() {
    let addr = spawn(test_config()).await;
    for (path, ct) in [
        ("/", "text/html"),
        ("/styles.css", "text/css"),
        ("/script.js", "text/javascript"),
        ("/worker.js", "text/javascript"),
        ("/lcf.wasm", "application/wasm"),
    ] {
        let r = http().get(format!("http://{addr}{path}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{path}");
        assert!(r.headers()["content-type"].to_str().unwrap().starts_with(ct), "{path}");
        assert!(r.headers()["cache-control"].to_str().unwrap().contains("no-cache"));
        let body = r.bytes().await.unwrap();
        if path == "/lcf.wasm" {
            assert_eq!(&body[..4], b"\0asm");
        }
    }
    let v = http().get(format!("http://{addr}/api/version")).send().await.unwrap();
    assert_eq!(v.text().await.unwrap(), local_clipboard::config::VERSION);
    let nf = http().get(format!("http://{addr}/nope")).send().await.unwrap();
    assert_eq!(nf.status(), 404);
}

#[tokio::test]
async fn qr_code_encodes_advertised_host() {
    let addr = spawn(test_config()).await;
    let r = http().get(format!("http://{addr}/qr")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "image/svg+xml");
    assert!(r.text().await.unwrap().contains("<svg"));

    let addr = spawn(local_clipboard::Config { advertised_host: None, ..test_config() }).await;
    let r = http().get(format!("http://{addr}/qr")).send().await.unwrap();
    assert_eq!(r.status(), 500);
}

#[tokio::test]
async fn control_endpoints_validate_input() {
    let addr = spawn(test_config()).await;
    let url = |p: &str| format!("http://{addr}{p}");
    for p in ["/clear", "/set-interval", "/toggle-pause"] {
        assert_eq!(http().get(url(p)).send().await.unwrap().status(), 405, "{p}");
    }
    let bad = http().post(url("/set-interval")).body("{").send().await.unwrap();
    assert_eq!(bad.status(), 400);
    let neg = http()
        .post(url("/set-interval"))
        .body(json!({"interval": -1}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(neg.status(), 400);

    let mut ws = connect(addr, "/ws", None).await;
    let first = wait_for(&mut ws, |m| m["type"] == "config").await;
    assert_eq!(first["config"]["intervalMin"], 10);
    assert!(first["config"]["nextClearTime"].as_str().unwrap().ends_with('Z'));

    let ok = http()
        .post(url("/set-interval"))
        .body(json!({"interval": 5}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 204);
    let c = wait_for(&mut ws, |m| m["type"] == "config").await;
    assert_eq!(c["config"]["intervalMin"], 5);

    assert_eq!(http().post(url("/toggle-pause")).send().await.unwrap().status(), 204);
    let c = wait_for(&mut ws, |m| m["type"] == "config").await;
    assert_eq!(c["config"]["paused"], true);

    assert_eq!(http().post(url("/clear")).send().await.unwrap().status(), 204);
    wait_for(&mut ws, |m| m["type"] == "clear").await;
    let c = wait_for(&mut ws, |m| m["type"] == "config").await;
    assert_eq!(c["config"]["paused"], true);
}

#[tokio::test]
async fn chat_broadcast_device_count_and_forwarded_ip() {
    let addr = spawn(test_config()).await;
    let mut a = connect(addr, "/ws", Some("192.168.1.20")).await;
    let hello = wait_for(&mut a, |m| m["type"] == "hello").await;
    assert_eq!(hello["ip"], "192.168.1.20");
    wait_for(&mut a, |m| m["type"] == "clients" && m["count"] == 1).await;

    // Two tabs on the same device count once.
    let _a2 = connect(addr, "/ws", Some("192.168.1.20")).await;
    wait_for(&mut a, |m| m["type"] == "clients" && m["count"] == 1).await;
    let mut b = connect(addr, "/ws", Some("192.168.1.21")).await;
    wait_for(&mut a, |m| m["type"] == "clients" && m["count"] == 2).await;

    send_json(&mut a, json!({"ref": "x1", "text": "héllo 👋"})).await;
    let got = wait_for(&mut b, |m| m["text"] == "héllo 👋").await;
    assert_eq!(got["senderIp"], "192.168.1.20");
    assert_eq!(got["ref"], "x1");
    assert!(got["file"].is_null());
    let echo = wait_for(&mut a, |m| m["ref"] == "x1").await;
    assert_eq!(echo["id"], got["id"]);

    // Empty messages are ignored.
    send_json(&mut a, json!({"text": ""})).await;
    send_json(&mut a, json!({"text": "after"})).await;
    let next = wait_for(&mut b, |m| m["text"].is_string()).await;
    assert_eq!(next["text"], "after");

    drop(b);
    wait_for(&mut a, |m| m["type"] == "clients" && m["count"] == 1).await;
}
