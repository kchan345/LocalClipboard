use std::net::SocketAddr;

use clap::Parser;
use local_clipboard::config::{normalize_args, Cli};
use local_clipboard::net_util::{advertised_host, can_open_browser, open_browser};
use local_clipboard::{serve, AppState};
use qrcode::render::unicode::Dense1x2;
use qrcode::{EcLevel, QrCode};

fn print_terminal_qr(url: &str) {
    if let Ok(code) = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::L) {
        let art = code
            .render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build();
        println!("\n{art}\n");
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .init();

    let cli = Cli::parse_from(normalize_args(std::env::args()));
    let host = advertised_host();
    let cfg = cli.to_config(host.clone());
    let port = cfg.port;

    let addr = SocketAddr::new(cli.bind, port);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Server failed to start on {addr}: {e}");
            std::process::exit(1);
        }
    };

    println!("📋 Local Clipboard v{}", cfg.version);
    println!("Server listening on {addr}");
    println!("Open http://localhost:{port} on this computer");
    match &host {
        Some(h) => {
            let url = format!("http://{h}:{port}");
            println!("Open {url} on your phone, or scan:");
            print_terminal_qr(&url);
        }
        None => println!("Open http://<this-computer-ip>:{port} on your phone"),
    }
    println!("Nothing is stored on this computer: text lives in the browsers, and files stay on the device that shared them.");
    println!("Press Ctrl+C to stop the server");

    let state = AppState::start(cfg);

    // The listener is already accepting connections, so the browser can be launched before serving starts.
    if cli.should_open() && can_open_browser() {
        if let Err(e) = open_browser(&format!("http://localhost:{port}")) {
            tracing::warn!("could not open browser automatically: {e}");
        }
    }

    tokio::select! {
        r = serve(listener, state) => {
            if let Err(e) = r {
                eprintln!("server error: {e}");
                std::process::exit(1);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down. All shared content is discarded.");
        }
    }
}
