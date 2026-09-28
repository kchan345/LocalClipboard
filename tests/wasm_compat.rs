//! Verifies that the browser codec (web/worker.js running web/lcf.wasm under
//! node) and the server's `lcf` crate agree on the LCF1 wire format.

use std::path::{Path, PathBuf};
use std::process::Command;

fn records(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut off = 0;
    while off < buf.len() {
        let n = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        out.push(&buf[off + 4..off + 4 + n]);
        off += 4 + n;
    }
    out
}

fn push(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn worker_wasm_codec_matches_rust() {
    if !node_available() {
        assert!(
            std::env::var_os("LCF_REQUIRE_NODE").is_none(),
            "node is required when LCF_REQUIRE_NODE is set"
        );
        eprintln!("skipping: node not found");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("lcf-compat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let rust_frames: PathBuf = dir.join("rust.bin");
    let js_frames: PathBuf = dir.join("js.bin");

    let mut fixtures: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"a".to_vec(),
        b"abc".repeat(30),
        "Grüße, 世界! "
            .repeat(20_000)
            .into_bytes()
            .into_iter()
            .take(lcf::MAX_CHUNK)
            .collect(),
        vec![0u8; lcf::MAX_CHUNK],
    ];
    let mut s = 0x9e3779b97f4a7c15u64;
    fixtures.push(
        (0..300_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect(),
    );
    let mut input = Vec::new();
    for f in &fixtures {
        for c in [true, false] {
            push(&mut input, f);
            push(&mut input, &lcf::encode_data(f, c).unwrap());
        }
    }
    std::fs::write(&rust_frames, &input).unwrap();

    let out = Command::new("node")
        .arg(root.join("tests").join("js").join("wasm_compat.mjs"))
        .arg(root)
        .arg(&rust_frames)
        .arg(&js_frames)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "node failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let buf = std::fs::read(&js_frames).unwrap();
    let recs = records(&buf);
    assert!(recs.len() >= 20 && recs.len() % 2 == 0);
    let mut lz4 = 0;
    for pair in recs.chunks(2) {
        let (orig, frame) = (pair[0], pair[1]);
        let f = lcf::parse(frame).unwrap();
        match f.kind {
            lcf::Kind::Raw | lcf::Kind::Lz4 => {
                lz4 += usize::from(f.kind == lcf::Kind::Lz4);
                assert_eq!(lcf::decode(&f).unwrap(), orig);
            }
            lcf::Kind::End | lcf::Kind::Error => {
                assert_eq!(f.payload, orig);
                assert_eq!(frame, lcf::encode_control(f.kind, orig).as_slice());
            }
            k => panic!("unexpected kind {k:?}"),
        }
    }
    assert!(lz4 >= 3);
    let _ = std::fs::remove_dir_all(&dir);
}
