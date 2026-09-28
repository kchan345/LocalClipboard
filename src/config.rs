//! Command-line interface and runtime configuration.

use std::net::IpAddr;
use std::time::Duration;

use clap::{ArgAction, Parser, ValueEnum};

/// Version string, overridable at build time (release CI injects the tag).
pub const VERSION: &str = match option_env!("LOCAL_CLIPBOARD_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TransferCompression {
    /// Senders lz4-compress chunks, skipping data that does not compress.
    Auto,
    /// Senders always send raw chunks.
    Off,
}

/// Share text, files and folders between devices on your local network.
#[derive(Debug, Parser)]
#[command(name = "local-clipboard", version = VERSION, about)]
pub struct Cli {
    /// Port to listen on.
    #[arg(long, short = 'p', default_value_t = 8080)]
    pub port: u16,

    /// Address to bind.
    #[arg(long, default_value = "0.0.0.0")]
    pub bind: IpAddr,

    /// Open the app in the default browser on startup (`--open=false` to disable).
    #[arg(long, default_value_t = true, num_args = 0..=1, default_missing_value = "true",
          action = ArgAction::Set, value_parser = clap::builder::BoolishValueParser::new())]
    pub open: bool,

    /// Do not open the browser on startup (same as `--open=false`).
    #[arg(long)]
    pub no_open: bool,

    /// Initial auto-clear interval in minutes (0 = never).
    #[arg(long, default_value_t = 10)]
    pub auto_clear: u32,

    /// On-the-fly lz4 compression of file transfers.
    #[arg(long, value_enum, default_value_t = TransferCompression::Auto)]
    pub transfer_compression: TransferCompression,

    /// Files up to this size are fetched compressed and decoded in the receiving
    /// browser (WASM); larger files are decoded by the server and streamed to disk.
    #[arg(long, default_value_t = 64)]
    pub browser_decode_max_mb: u64,

    /// Maximum concurrent downloads served by one sending device.
    #[arg(long, default_value_t = 4)]
    pub max_transfers_per_sender: usize,

    /// Seconds to wait for the sending device to start a transfer.
    #[arg(long, default_value_t = 30)]
    pub transfer_timeout: u64,
}

/// Rewrites Go-style single-dash long flags (`-port 3000`, `-open=false`) to
/// their `--` form so the reference implementation's command lines keep working.
pub fn normalize_args<I: IntoIterator<Item = String>>(args: I) -> Vec<String> {
    let mut out = Vec::new();
    for (i, a) in args.into_iter().enumerate() {
        let is_go_long = i > 0
            && a.starts_with('-')
            && !a.starts_with("--")
            && a.len() > 2
            && a.as_bytes()[1].is_ascii_alphabetic()
            && (a.as_bytes()[2].is_ascii_alphabetic() || a.as_bytes()[2] == b'-');
        if is_go_long {
            out.push(format!("-{a}"));
        } else {
            out.push(a);
        }
    }
    out
}

/// Runtime configuration shared by the server components.
#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub version: String,
    /// Host advertised in the QR code / banner (LOCAL_CLIPBOARD_HOST or LAN IP).
    pub advertised_host: Option<String>,
    pub auto_clear_min: u32,
    /// Length of one auto-clear "minute" (shortened in tests).
    pub clear_unit: Duration,
    pub transfer_compression: bool,
    pub browser_decode_max_bytes: u64,
    pub max_transfers_per_sender: usize,
    pub transfer_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 8080,
            version: VERSION.to_string(),
            advertised_host: None,
            auto_clear_min: 10,
            clear_unit: Duration::from_secs(60),
            transfer_compression: true,
            browser_decode_max_bytes: 64 << 20,
            max_transfers_per_sender: 4,
            transfer_timeout: Duration::from_secs(30),
        }
    }
}

impl Cli {
    /// `true` when the browser should be opened, honouring flags and env.
    pub fn should_open(&self) -> bool {
        self.open
            && !self.no_open
            && !matches!(std::env::var_os("LOCAL_CLIPBOARD_NO_OPEN"), Some(v) if !v.is_empty())
    }

    pub fn to_config(&self, advertised_host: Option<String>) -> Config {
        Config {
            port: self.port,
            advertised_host,
            auto_clear_min: self.auto_clear,
            transfer_compression: self.transfer_compression == TransferCompression::Auto,
            browser_decode_max_bytes: self.browser_decode_max_mb.saturating_mul(1 << 20),
            max_transfers_per_sender: self.max_transfers_per_sender.max(1),
            transfer_timeout: Duration::from_secs(self.transfer_timeout.max(1)),
            ..Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        let argv = std::iter::once("local-clipboard")
            .chain(args.iter().copied())
            .map(String::from);
        Cli::try_parse_from(normalize_args(argv)).unwrap()
    }

    #[test]
    fn defaults() {
        let c = parse(&[]);
        assert_eq!(c.port, 8080);
        assert!(c.open && !c.no_open);
        assert_eq!(c.auto_clear, 10);
        assert_eq!(c.transfer_compression, TransferCompression::Auto);
    }

    #[test]
    fn go_style_flags() {
        let c = parse(&["-port", "3000", "-open=false"]);
        assert_eq!(c.port, 3000);
        assert!(!c.open);
        let c = parse(&["-port=4000", "-open"]);
        assert_eq!(c.port, 4000);
        assert!(c.open);
    }

    #[test]
    fn gnu_style_flags() {
        let c = parse(&["-p", "9001", "--no-open", "--transfer-compression", "off"]);
        assert_eq!(c.port, 9001);
        assert!(c.no_open);
        assert!(!c.should_open());
        assert_eq!(c.transfer_compression, TransferCompression::Off);
        let c = parse(&["--open", "0"]);
        assert!(!c.open);
    }

    #[test]
    fn normalization_leaves_values_alone() {
        let v = normalize_args(["x", "-p", "5", "--port", "-1", "-open=false"].map(String::from));
        assert_eq!(v, ["x", "-p", "5", "--port", "-1", "--open=false"]);
    }
}
