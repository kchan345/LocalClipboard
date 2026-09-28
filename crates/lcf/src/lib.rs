//! LCF1 — the LocalClipboard transfer framing.
//!
//! A transfer is a sequence of self-delimiting frames. Every frame is
//!
//! ```text
//! +--------+-------------------+-----------------------+-----------------+
//! | kind u8| raw_len u32 (LE)  | payload_len u32 (LE)  | payload bytes   |
//! +--------+-------------------+-----------------------+-----------------+
//! ```
//!
//! * `Raw`      – payload is `raw_len` uncompressed bytes.
//! * `Lz4`      – payload is an lz4 *block* that decompresses to exactly `raw_len` bytes.
//! * `Manifest` – UTF-8 JSON (directory listing), `raw_len == payload_len`.
//! * `End`      – end of stream, empty.
//! * `Error`    – sender aborted, UTF-8 reason.
//!
//! Every data chunk is compressed independently (no shared dictionary), so a
//! receiver only ever needs one chunk in memory and `raw_len` is bounded by
//! [`MAX_CHUNK`], which also caps the cost of a malicious "decompression bomb".
//!
//! The same crate is compiled natively into the server and to
//! `wasm32-unknown-unknown` for the browser, so both sides share one codec.

#![forbid(unsafe_code)]

use std::fmt;

/// Size of the fixed frame header.
pub const HEADER_LEN: usize = 9;
/// Largest uncompressed payload a data frame may carry (1 MiB).
pub const MAX_CHUNK: usize = 1 << 20;
/// Largest control payload (manifest / error) (16 MiB).
pub const MAX_CONTROL: usize = 16 << 20;
/// Chunks smaller than this are never worth compressing.
pub const MIN_COMPRESS: usize = 64;

/// Frame kind tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Raw = 0,
    Lz4 = 1,
    Manifest = 2,
    End = 3,
    Error = 4,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Kind::Raw,
            1 => Kind::Lz4,
            2 => Kind::Manifest,
            3 => Kind::End,
            4 => Kind::Error,
            _ => return None,
        })
    }

    /// `true` for frames carrying file bytes.
    pub fn is_data(self) -> bool {
        matches!(self, Kind::Raw | Kind::Lz4)
    }
}

/// Framing / codec errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    TooShort,
    UnknownKind(u8),
    LengthMismatch,
    TooLarge(usize),
    Corrupt,
    OutputTooSmall,
    NotData,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooShort => write!(f, "frame shorter than header"),
            FrameError::UnknownKind(k) => write!(f, "unknown frame kind {k}"),
            FrameError::LengthMismatch => write!(f, "frame length fields do not match payload"),
            FrameError::TooLarge(n) => write!(f, "frame too large ({n} bytes)"),
            FrameError::Corrupt => write!(f, "corrupt lz4 block"),
            FrameError::OutputTooSmall => write!(f, "output buffer too small"),
            FrameError::NotData => write!(f, "not a data frame"),
        }
    }
}

impl std::error::Error for FrameError {}

/// A parsed, validated frame borrowing its payload.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub kind: Kind,
    pub raw_len: usize,
    pub payload: &'a [u8],
}

fn read_u32(b: &[u8]) -> usize {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize
}

fn write_header(out: &mut [u8], kind: Kind, raw_len: usize, payload_len: usize) {
    out[0] = kind as u8;
    out[1..5].copy_from_slice(&(raw_len as u32).to_le_bytes());
    out[5..9].copy_from_slice(&(payload_len as u32).to_le_bytes());
}

/// Parses and validates exactly one frame occupying all of `buf`.
pub fn parse(buf: &[u8]) -> Result<Frame<'_>, FrameError> {
    if buf.len() < HEADER_LEN {
        return Err(FrameError::TooShort);
    }
    let kind = Kind::from_u8(buf[0]).ok_or(FrameError::UnknownKind(buf[0]))?;
    let raw_len = read_u32(&buf[1..5]);
    let payload_len = read_u32(&buf[5..9]);
    let payload = &buf[HEADER_LEN..];
    if payload.len() != payload_len {
        return Err(FrameError::LengthMismatch);
    }
    match kind {
        Kind::Raw => {
            if raw_len > MAX_CHUNK {
                return Err(FrameError::TooLarge(raw_len));
            }
            if raw_len != payload_len {
                return Err(FrameError::LengthMismatch);
            }
        }
        Kind::Lz4 => {
            if raw_len > MAX_CHUNK {
                return Err(FrameError::TooLarge(raw_len));
            }
            if payload_len > lz4_flex::block::get_maximum_output_size(raw_len) {
                return Err(FrameError::LengthMismatch);
            }
        }
        Kind::Manifest | Kind::Error => {
            if raw_len > MAX_CONTROL {
                return Err(FrameError::TooLarge(raw_len));
            }
            if raw_len != payload_len {
                return Err(FrameError::LengthMismatch);
            }
        }
        Kind::End => {
            if raw_len != 0 || payload_len != 0 {
                return Err(FrameError::LengthMismatch);
            }
        }
    }
    Ok(Frame {
        kind,
        raw_len,
        payload,
    })
}

/// Worst-case encoded size of a data frame for `n` input bytes.
pub fn max_frame_len(n: usize) -> usize {
    HEADER_LEN + lz4_flex::block::get_maximum_output_size(n).max(n)
}

/// Encodes one data chunk into `out`, returning the frame length.
///
/// When `try_compress` is set the chunk is lz4-compressed, but it falls back to
/// a `Raw` frame if compression saves less than 10 %, so incompressible input
/// costs one failed attempt and nothing on the decode side.
pub fn encode_data_into(
    input: &[u8],
    try_compress: bool,
    out: &mut [u8],
) -> Result<usize, FrameError> {
    let n = input.len();
    if n > MAX_CHUNK {
        return Err(FrameError::TooLarge(n));
    }
    if out.len() < max_frame_len(n) {
        return Err(FrameError::OutputTooSmall);
    }
    if try_compress && n >= MIN_COMPRESS {
        if let Ok(c) = lz4_flex::block::compress_into(input, &mut out[HEADER_LEN..]) {
            if c * 10 < n * 9 {
                write_header(out, Kind::Lz4, n, c);
                return Ok(HEADER_LEN + c);
            }
        }
    }
    out[HEADER_LEN..HEADER_LEN + n].copy_from_slice(input);
    write_header(out, Kind::Raw, n, n);
    Ok(HEADER_LEN + n)
}

/// Allocating convenience wrapper around [`encode_data_into`].
pub fn encode_data(input: &[u8], try_compress: bool) -> Result<Vec<u8>, FrameError> {
    let mut out = vec![0u8; max_frame_len(input.len())];
    let n = encode_data_into(input, try_compress, &mut out)?;
    out.truncate(n);
    Ok(out)
}

/// Encodes a control frame (`Manifest`, `End`, `Error`).
pub fn encode_control(kind: Kind, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_LEN + payload.len()];
    write_header(&mut out, kind, payload.len(), payload.len());
    out[HEADER_LEN..].copy_from_slice(payload);
    out
}

/// Decodes a data frame into `out[..raw_len]`, returning `raw_len`.
pub fn decode_into(frame: &Frame<'_>, out: &mut [u8]) -> Result<usize, FrameError> {
    if out.len() < frame.raw_len {
        return Err(FrameError::OutputTooSmall);
    }
    match frame.kind {
        Kind::Raw => {
            out[..frame.raw_len].copy_from_slice(frame.payload);
            Ok(frame.raw_len)
        }
        Kind::Lz4 => {
            let n = lz4_flex::block::decompress_into(frame.payload, &mut out[..frame.raw_len])
                .map_err(|_| FrameError::Corrupt)?;
            if n != frame.raw_len {
                return Err(FrameError::Corrupt);
            }
            Ok(n)
        }
        _ => Err(FrameError::NotData),
    }
}

/// Allocating convenience wrapper around [`decode_into`].
pub fn decode(frame: &Frame<'_>) -> Result<Vec<u8>, FrameError> {
    let mut out = vec![0u8; frame.raw_len];
    decode_into(frame, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(n: usize, mut seed: u64) -> Vec<u8> {
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    #[test]
    fn compressible_roundtrip_uses_lz4() {
        let input: Vec<u8> = b"hello local clipboard! "
            .iter()
            .cycle()
            .take(200_000)
            .copied()
            .collect();
        let frame = encode_data(&input, true).unwrap();
        let parsed = parse(&frame).unwrap();
        assert_eq!(parsed.kind, Kind::Lz4);
        assert!(frame.len() < input.len() / 10);
        assert_eq!(decode(&parsed).unwrap(), input);
    }

    #[test]
    fn incompressible_falls_back_to_raw() {
        let input = pseudo_random(100_000, 42);
        let frame = encode_data(&input, true).unwrap();
        let parsed = parse(&frame).unwrap();
        assert_eq!(parsed.kind, Kind::Raw);
        assert_eq!(decode(&parsed).unwrap(), input);
    }

    #[test]
    fn compression_can_be_disabled_and_tiny_chunks_stay_raw() {
        let input = vec![0u8; 4096];
        assert_eq!(
            parse(&encode_data(&input, false).unwrap()).unwrap().kind,
            Kind::Raw
        );
        let tiny = vec![0u8; MIN_COMPRESS - 1];
        assert_eq!(
            parse(&encode_data(&tiny, true).unwrap()).unwrap().kind,
            Kind::Raw
        );
    }

    #[test]
    fn empty_and_max_chunks() {
        let f = encode_data(&[], true).unwrap();
        assert_eq!(decode(&parse(&f).unwrap()).unwrap(), Vec::<u8>::new());
        let big = vec![7u8; MAX_CHUNK];
        let f = encode_data(&big, true).unwrap();
        assert_eq!(decode(&parse(&f).unwrap()).unwrap(), big);
        assert_eq!(
            encode_data(&vec![0u8; MAX_CHUNK + 1], true),
            Err(FrameError::TooLarge(MAX_CHUNK + 1))
        );
    }

    #[test]
    fn control_frames() {
        let m = encode_control(Kind::Manifest, b"[]");
        let p = parse(&m).unwrap();
        assert_eq!(p.kind, Kind::Manifest);
        assert_eq!(p.payload, b"[]");
        let e = encode_control(Kind::End, &[]);
        assert_eq!(parse(&e).unwrap().kind, Kind::End);
        assert_eq!(decode(&parse(&e).unwrap()), Err(FrameError::NotData));
    }

    #[test]
    fn rejects_malformed_frames() {
        assert_eq!(parse(&[0, 0, 0]).unwrap_err(), FrameError::TooShort);
        let mut f = encode_data(b"abc", false).unwrap();
        f[0] = 9;
        assert_eq!(parse(&f).unwrap_err(), FrameError::UnknownKind(9));
        let mut f = encode_data(b"abc", false).unwrap();
        f.push(0);
        assert_eq!(parse(&f).unwrap_err(), FrameError::LengthMismatch);
        // lz4 frame claiming a huge raw length (decompression bomb)
        let mut bomb = encode_data(&vec![0u8; 10_000], true).unwrap();
        bomb[1..5].copy_from_slice(&((MAX_CHUNK as u32) + 1).to_le_bytes());
        assert!(matches!(parse(&bomb), Err(FrameError::TooLarge(_))));
        // lz4 frame whose raw_len lies about the real decompressed size
        let mut liar = encode_data(&vec![0u8; 10_000], true).unwrap();
        liar[1..5].copy_from_slice(&9_999u32.to_le_bytes());
        let p = parse(&liar).unwrap();
        assert!(decode(&p).is_err());
        let mut end = encode_control(Kind::End, &[]);
        end[1] = 1;
        assert_eq!(parse(&end).unwrap_err(), FrameError::LengthMismatch);
    }
}
