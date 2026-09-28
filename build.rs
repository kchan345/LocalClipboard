use std::path::Path;

fn main() {
    let wasm = Path::new("web/lcf.wasm");
    println!("cargo:rerun-if-changed=web/lcf.wasm");
    println!("cargo:rerun-if-env-changed=LOCAL_CLIPBOARD_VERSION");
    if !wasm.exists() {
        panic!(
            "web/lcf.wasm is missing. Build the browser codec first:\n    \
             cargo build -p lcf-wasm --target wasm32-unknown-unknown --release\n    \
             cp target/wasm32-unknown-unknown/release/lcf_wasm.wasm web/lcf.wasm\n\
             (scripts/build-wasm.sh does both; CI runs it automatically)"
        );
    }
}
