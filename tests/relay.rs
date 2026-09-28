mod common;

use std::io::{Cursor, Read};
use std::time::Duration;

use common::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

async fn get(url: String) -> reqwest::Response {
    reqwest::Client::new().get(url).send().await.unwrap()
}

#[tokio::test]
async fn single_file_is_relayed_and_decoded() {
    let addr = spawn(test_config()).await;
    let mut owner = connect(addr, "/ws", None).await;
    let mut data = text_payload(700_000);
    data.extend(random_payload(300_000, 7));
    let id = share(
        &mut owner,
        json!({"name": "notes – 2024.txt", "size": data.len(), "type": "text/plain"}),
    )
    .await;

    let dl = tokio::spawn(get(format!("http://{addr}/file/{id}")));
    let (token, mode) = wait_request(&mut owner, &id).await;
    assert_eq!(mode, "plain");
    let mut relay = connect(addr, &format!("/relay/{token}"), None).await;
    send_frames(&mut relay, &data, 256 * 1024).await;
    send_end(&mut relay).await;

    let r = dl.await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["content-length"],
        data.len().to_string().as_str()
    );
    assert_eq!(r.headers()["content-type"], "text/plain");
    let cd = r.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(cd.starts_with("attachment;"), "{cd}");
    assert!(
        cd.contains("filename*=UTF-8''notes%20%E2%80%93%202024.txt"),
        "{cd}"
    );
    let body = r.bytes().await.unwrap();
    assert!(body[..] == data[..], "body mismatch");

    // The token is single use.
    let reuse = tokio_tungstenite::connect_async(format!("ws://{addr}/relay/{token}")).await;
    assert!(reuse.is_err());
}

#[tokio::test]
async fn empty_file() {
    let addr = spawn(test_config()).await;
    let mut owner = connect(addr, "/ws", None).await;
    let id = share(&mut owner, json!({"name": "empty.bin", "size": 0})).await;
    let dl = tokio::spawn(get(format!("http://{addr}/file/{id}")));
    let (token, _) = wait_request(&mut owner, &id).await;
    let mut relay = connect(addr, &format!("/relay/{token}"), None).await;
    send_end(&mut relay).await;
    let r = dl.await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    assert!(r.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn passthrough_pull_keeps_frames_compressed() {
    let addr = spawn(test_config()).await;
    let mut owner = connect(addr, "/ws", None).await;
    let data = text_payload(600_000);
    let id = share(&mut owner, json!({"name": "a.txt", "size": data.len()})).await;

    let mut pull = connect(addr, &format!("/pull/{id}"), None).await;
    let (token, mode) = wait_request(&mut owner, &id).await;
    assert_eq!(mode, "passthrough");
    let mut relay = connect(addr, &format!("/relay/{token}"), None).await;
    let sender = tokio::spawn(async move {
        send_frames(&mut relay, &data, 128 * 1024).await;
        send_end(&mut relay).await;
        data
    });

    let mut out = Vec::new();
    let mut wire = 0usize;
    let mut compressed_frames = 0;
    loop {
        match pull.next().await.unwrap().unwrap() {
            Message::Binary(b) => {
                wire += b.len();
                let f = lcf::parse(&b).unwrap();
                match f.kind {
                    lcf::Kind::End => break,
                    lcf::Kind::Lz4 => {
                        compressed_frames += 1;
                        out.extend(lcf::decode(&f).unwrap());
                    }
                    lcf::Kind::Raw => out.extend_from_slice(f.payload),
                    k => panic!("unexpected frame {k:?}"),
                }
            }
            other => panic!("unexpected message {other:?}"),
        }
    }
    let data = sender.await.unwrap();
    assert!(out == data);
    assert!(compressed_frames > 0);
    assert!(wire < data.len() / 2, "wire {wire} vs raw {}", data.len());
    match pull.next().await {
        Some(Ok(Message::Close(Some(c)))) => assert_eq!(u16::from(c.code), 1000),
        other => panic!("expected close, got {other:?}"),
    }
}

#[tokio::test]
async fn folder_is_streamed_as_zip() {
    let addr = spawn(test_config()).await;
    let mut owner = connect(addr, "/ws", None).await;
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("photos/a.txt", text_payload(300_000)),
        ("photos/sub/b.bin", random_payload(200_001, 3)),
        ("photos/zero.txt", Vec::new()),
        ("photos/ünï.txt", b"unicode name".to_vec()),
    ];
    let mut manifest: Vec<serde_json::Value> = files
        .iter()
        .map(|(p, d)| json!({"path": p, "size": d.len(), "mtime": 1_700_000_000_000i64}))
        .collect();
    manifest.push(json!({"path": "photos/empty", "dir": true}));
    let total: usize = files.iter().map(|(_, d)| d.len()).sum();
    let id = share(
        &mut owner,
        json!({"name": "photos", "kind": "dir", "size": total, "count": files.len()}),
    )
    .await;

    let dl = tokio::spawn(get(format!("http://{addr}/file/{id}")));
    let (token, _) = wait_request(&mut owner, &id).await;
    let mut relay = connect(addr, &format!("/relay/{token}"), None).await;
    let mf = lcf::encode_control(lcf::Kind::Manifest, json!(manifest).to_string().as_bytes());
    relay.send(Message::Binary(mf.into())).await.unwrap();
    // Chunk boundaries deliberately straddle file boundaries.
    let concat: Vec<u8> = files.iter().flat_map(|(_, d)| d.iter().copied()).collect();
    send_frames(&mut relay, &concat, 70_000).await;
    send_end(&mut relay).await;

    let r = dl.await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/zip");
    let declared: usize = r.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(r.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains("photos.zip"));
    let body = r.bytes().await.unwrap();
    assert_eq!(body.len(), declared);

    let mut zip = zip::ZipArchive::new(Cursor::new(body.to_vec())).unwrap();
    for (path, data) in &files {
        let mut e = zip
            .by_name(path)
            .unwrap_or_else(|_| panic!("missing {path}"));
        let mut got = Vec::new();
        e.read_to_end(&mut got).unwrap();
        assert!(&got == data, "{path} differs");
    }
    let dir = zip.by_name("photos/empty/").unwrap();
    assert!(dir.is_dir());
}

#[tokio::test]
async fn owner_disconnect_makes_attachments_unavailable() {
    let addr = spawn(test_config()).await;
    let mut owner = connect(addr, "/ws", Some("10.0.0.2")).await;
    let mut viewer = connect(addr, "/ws", Some("10.0.0.3")).await;
    let id = share(&mut owner, json!({"name": "x.bin", "size": 10})).await;
    wait_for(&mut viewer, |m| m["id"] == id.as_str()).await;
    owner.close(None).await.unwrap();
    let m = wait_for(&mut viewer, |m| m["type"] == "unavailable").await;
    assert_eq!(m["ids"], json!([id]));
    assert_eq!(get(format!("http://{addr}/file/{id}")).await.status(), 410);
    assert_eq!(
        get(format!("http://{addr}/file/unknown")).await.status(),
        404
    );
}

#[tokio::test]
async fn mid_transfer_abort_fails_the_download() {
    let addr = spawn(test_config()).await;
    let mut owner = connect(addr, "/ws", None).await;
    let data = random_payload(2_000_000, 11);
    let id = share(&mut owner, json!({"name": "big.bin", "size": data.len()})).await;
    let dl = tokio::spawn(get(format!("http://{addr}/file/{id}")));
    let (token, _) = wait_request(&mut owner, &id).await;
    let mut relay = connect(addr, &format!("/relay/{token}"), None).await;
    send_frames(&mut relay, &data[..500_000], 100_000).await;
    let err = lcf::encode_control(lcf::Kind::Error, b"NotReadableError");
    relay.send(Message::Binary(err.into())).await.unwrap();
    let r = dl.await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.bytes().await.is_err(), "truncated download must fail");
}

#[tokio::test]
async fn oversized_stream_is_rejected() {
    let addr = spawn(test_config()).await;
    let mut owner = connect(addr, "/ws", None).await;
    let id = share(&mut owner, json!({"name": "s.txt", "size": 1000})).await;
    let dl = tokio::spawn(get(format!("http://{addr}/file/{id}")));
    let (token, _) = wait_request(&mut owner, &id).await;
    let mut relay = connect(addr, &format!("/relay/{token}"), None).await;
    send_frames(&mut relay, &text_payload(5000), 5000).await;
    let r = dl.await.unwrap();
    assert!(r.bytes().await.is_err());
}

#[tokio::test]
async fn unresponsive_owner_times_out() {
    let cfg = local_clipboard::Config {
        transfer_timeout: Duration::from_millis(500),
        ..test_config()
    };
    let addr = spawn(cfg).await;
    let mut owner = connect(addr, "/ws", None).await;
    let id = share(&mut owner, json!({"name": "t.txt", "size": 1})).await;
    let r = get(format!("http://{addr}/file/{id}")).await;
    assert_eq!(r.status(), 504);
    let (token, _) = wait_request(&mut owner, &id).await;
    // The abandoned token can no longer be used.
    assert!(
        tokio_tungstenite::connect_async(format!("ws://{addr}/relay/{token}"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn per_sender_concurrency_limit() {
    let cfg = local_clipboard::Config {
        max_transfers_per_sender: 1,
        ..test_config()
    };
    let addr = spawn(cfg).await;
    let mut owner = connect(addr, "/ws", None).await;
    let id = share(&mut owner, json!({"name": "c.txt", "size": 3})).await;
    let first = tokio::spawn(get(format!("http://{addr}/file/{id}")));
    let (token, _) = wait_request(&mut owner, &id).await;
    assert_eq!(get(format!("http://{addr}/file/{id}")).await.status(), 429);
    let mut relay = connect(addr, &format!("/relay/{token}"), None).await;
    send_frames(&mut relay, b"abc", 3).await;
    send_end(&mut relay).await;
    assert_eq!(&first.await.unwrap().bytes().await.unwrap()[..], b"abc");
}

#[tokio::test]
async fn unknown_relay_token_is_rejected() {
    let addr = spawn(test_config()).await;
    let err = tokio_tungstenite::connect_async(format!("ws://{addr}/relay/deadbeef"))
        .await
        .unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(r) => assert_eq!(r.status(), 404),
        e => panic!("unexpected error {e:?}"),
    }
}
