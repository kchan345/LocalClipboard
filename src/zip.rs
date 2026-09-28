//! Streaming ZIP writer used to deliver shared folders as a single download.
//!
//! * Method STORE only: data already crossed the network lz4-compressed and the
//!   server → receiver hop is LAN; deflating would cost server CPU for little gain.
//! * Because STORE output size is fully determined by the manifest (names and
//!   sizes), [`Plan::total_len`] gives the exact archive size *before* any data
//!   arrives, so the HTTP response carries a real `Content-Length`.
//! * CRC-32 is not known up front, so entries with data use a data descriptor
//!   (general purpose flag bit 3), exactly like Go's `archive/zip` streaming writer.
//! * ZIP64 records are emitted only where a size, offset or entry count needs them.

use std::collections::HashSet;
use std::fmt;

use bytes::Bytes;
use serde::Deserialize;

use crate::timefmt::dos_datetime;

/// Maximum number of manifest entries accepted for one folder.
pub const MAX_ENTRIES: usize = 100_000;
/// Maximum length of a path inside the archive, in bytes.
pub const MAX_PATH: usize = 4096;

const U32_MAX: u64 = 0xFFFF_FFFF;
const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
const FLAG_UTF8: u16 = 0x0800;

/// One manifest entry as sent by the browser.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub dir: bool,
    /// Last modification time, Unix milliseconds.
    #[serde(default)]
    pub mtime: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZipError {
    TooManyEntries,
    BadPath(String),
    Duplicate(String),
    DirWithData(String),
    TooMuchData,
    Truncated,
}

impl fmt::Display for ZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZipError::TooManyEntries => write!(f, "too many entries (max {MAX_ENTRIES})"),
            ZipError::BadPath(p) => write!(f, "unsafe or invalid path: {p:?}"),
            ZipError::Duplicate(p) => write!(f, "duplicate path: {p:?}"),
            ZipError::DirWithData(p) => write!(f, "directory entry with data: {p:?}"),
            ZipError::TooMuchData => write!(f, "more data than the manifest announced"),
            ZipError::Truncated => write!(f, "stream ended before all files were received"),
        }
    }
}

impl std::error::Error for ZipError {}

/// Normalises a relative path and rejects anything that could escape the
/// extraction directory ("zip slip"): absolute paths, drive letters, `..`.
pub fn sanitize_path(path: &str) -> Result<String, ZipError> {
    let bad = || ZipError::BadPath(path.to_string());
    if path.contains('\0') {
        return Err(bad());
    }
    let unified = path.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(bad());
    }
    let bytes = unified.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(bad());
    }
    let mut parts = Vec::new();
    for part in unified.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(bad()),
            p if p.chars().any(|c| c.is_control()) => return Err(bad()),
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return Err(bad());
    }
    let out = parts.join("/");
    if out.len() > MAX_PATH {
        return Err(bad());
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct PlannedEntry {
    name: String,
    size: u64,
    is_dir: bool,
    time: u16,
    date: u16,
    offset: u64,
    z64_size: bool,
    z64_off: bool,
}

impl PlannedEntry {
    fn has_descriptor(&self) -> bool {
        self.size > 0
    }
    fn flags(&self) -> u16 {
        if self.has_descriptor() {
            FLAG_UTF8 | FLAG_DATA_DESCRIPTOR
        } else {
            FLAG_UTF8
        }
    }
    fn version(&self) -> u16 {
        if self.z64_size || self.z64_off {
            45
        } else {
            20
        }
    }
    fn local_len(&self) -> u64 {
        30 + self.name.len() as u64 + if self.z64_size { 20 } else { 0 }
    }
    fn descriptor_len(&self) -> u64 {
        match (self.has_descriptor(), self.z64_size) {
            (false, _) => 0,
            (true, true) => 24,
            (true, false) => 16,
        }
    }
    fn central_extra_len(&self) -> u64 {
        let n = if self.z64_size { 16 } else { 0 } + if self.z64_off { 8 } else { 0 };
        if n > 0 {
            4 + n
        } else {
            0
        }
    }
    fn central_len(&self) -> u64 {
        46 + self.name.len() as u64 + self.central_extra_len()
    }
}

/// Precomputed archive layout.
#[derive(Debug, Clone)]
pub struct Plan {
    entries: Vec<PlannedEntry>,
    cd_offset: u64,
    cd_size: u64,
    zip64_end: bool,
    total_len: u64,
    data_len: u64,
}

impl Plan {
    pub fn new(manifest: &[ManifestEntry]) -> Result<Plan, ZipError> {
        if manifest.len() > MAX_ENTRIES {
            return Err(ZipError::TooManyEntries);
        }
        let mut seen = HashSet::with_capacity(manifest.len());
        let mut entries = Vec::with_capacity(manifest.len());
        let mut offset = 0u64;
        let mut data_len = 0u64;
        for m in manifest {
            let mut name = sanitize_path(&m.path)?;
            if m.dir {
                if m.size != 0 {
                    return Err(ZipError::DirWithData(name));
                }
                name.push('/');
            }
            if !seen.insert(name.clone()) {
                return Err(ZipError::Duplicate(name));
            }
            let (time, date) = dos_datetime(m.mtime);
            let e = PlannedEntry {
                name,
                size: m.size,
                is_dir: m.dir,
                time,
                date,
                offset,
                z64_size: m.size >= U32_MAX,
                z64_off: offset >= U32_MAX,
            };
            offset += e.local_len() + e.size + e.descriptor_len();
            data_len += e.size;
            entries.push(e);
        }
        let cd_offset = offset;
        let cd_size: u64 = entries.iter().map(PlannedEntry::central_len).sum();
        let zip64_end = entries.len() >= 0xFFFF || cd_offset >= U32_MAX || cd_size >= U32_MAX;
        let total_len = cd_offset + cd_size + if zip64_end { 56 + 20 } else { 0 } + 22;
        Ok(Plan {
            entries,
            cd_offset,
            cd_size,
            zip64_end,
            total_len,
            data_len,
        })
    }

    /// Exact size of the finished archive in bytes.
    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Total file payload the sender must stream.
    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn write_local(b: &mut Vec<u8>, e: &PlannedEntry) {
    put32(b, 0x0403_4b50);
    put16(b, e.version());
    put16(b, e.flags());
    put16(b, 0); // method: stored
    put16(b, e.time);
    put16(b, e.date);
    put32(b, 0); // crc: in the data descriptor (or 0 for empty entries)
    let sz = if e.z64_size { 0xFFFF_FFFF } else { 0 };
    put32(b, sz);
    put32(b, sz);
    put16(b, e.name.len() as u16);
    put16(b, if e.z64_size { 20 } else { 0 });
    b.extend_from_slice(e.name.as_bytes());
    if e.z64_size {
        put16(b, 0x0001);
        put16(b, 16);
        put64(b, 0);
        put64(b, 0);
    }
}

fn write_descriptor(b: &mut Vec<u8>, e: &PlannedEntry, crc: u32) {
    put32(b, 0x0807_4b50);
    put32(b, crc);
    if e.z64_size {
        put64(b, e.size);
        put64(b, e.size);
    } else {
        put32(b, e.size as u32);
        put32(b, e.size as u32);
    }
}

fn write_central(b: &mut Vec<u8>, e: &PlannedEntry, crc: u32) {
    put32(b, 0x0201_4b50);
    put16(b, 0x0300 | e.version()); // made by: Unix
    put16(b, e.version());
    put16(b, e.flags());
    put16(b, 0);
    put16(b, e.time);
    put16(b, e.date);
    put32(b, crc);
    let sz = if e.z64_size {
        0xFFFF_FFFF
    } else {
        e.size as u32
    };
    put32(b, sz);
    put32(b, sz);
    put16(b, e.name.len() as u16);
    put16(b, e.central_extra_len() as u16);
    put16(b, 0); // comment
    put16(b, 0); // disk start
    put16(b, 0); // internal attrs
    let external = if e.is_dir {
        (0o40755u32 << 16) | 0x10
    } else {
        0o100644u32 << 16
    };
    put32(b, external);
    put32(
        b,
        if e.z64_off {
            0xFFFF_FFFF
        } else {
            e.offset as u32
        },
    );
    b.extend_from_slice(e.name.as_bytes());
    let extra = e.central_extra_len();
    if extra > 0 {
        put16(b, 0x0001);
        put16(b, (extra - 4) as u16);
        if e.z64_size {
            put64(b, e.size);
            put64(b, e.size);
        }
        if e.z64_off {
            put64(b, e.offset);
        }
    }
}

/// Incremental archive generator: `begin`, then `feed` file bytes in manifest
/// order (chunk boundaries do not need to align with file boundaries), then `finish`.
pub struct ZipStream {
    plan: Plan,
    idx: usize,
    remaining: u64,
    hasher: crc32fast::Hasher,
    crcs: Vec<u32>,
    written: u64,
}

impl ZipStream {
    pub fn new(plan: Plan) -> Self {
        let n = plan.entries.len();
        ZipStream {
            plan,
            idx: 0,
            remaining: 0,
            hasher: crc32fast::Hasher::new(),
            crcs: Vec::with_capacity(n),
            written: 0,
        }
    }

    /// Emits local headers until an entry that needs data (or the end) is reached.
    fn advance(&mut self, buf: &mut Vec<u8>) {
        while self.idx < self.plan.entries.len() {
            let e = &self.plan.entries[self.idx];
            write_local(buf, e);
            if e.size == 0 {
                self.crcs.push(0);
                self.idx += 1;
            } else {
                self.remaining = e.size;
                self.hasher = crc32fast::Hasher::new();
                return;
            }
        }
    }

    fn emit(&mut self, out: &mut Vec<Bytes>, b: Bytes) {
        if !b.is_empty() {
            self.written += b.len() as u64;
            out.push(b);
        }
    }

    /// Starts the archive; returns the leading headers.
    pub fn begin(&mut self) -> Vec<Bytes> {
        let mut buf = Vec::new();
        self.advance(&mut buf);
        let mut out = Vec::new();
        self.emit(&mut out, buf.into());
        out
    }

    /// `true` once every entry has received all its bytes.
    pub fn data_complete(&self) -> bool {
        self.idx >= self.plan.entries.len()
    }

    /// Appends file bytes, returning archive bytes to send.
    pub fn feed(&mut self, mut data: Bytes) -> Result<Vec<Bytes>, ZipError> {
        let mut out = Vec::new();
        while !data.is_empty() {
            if self.data_complete() {
                return Err(ZipError::TooMuchData);
            }
            let take = self.remaining.min(data.len() as u64) as usize;
            let chunk = data.split_to(take);
            self.hasher.update(&chunk);
            self.remaining -= take as u64;
            self.emit(&mut out, chunk);
            if self.remaining == 0 {
                let crc = std::mem::replace(&mut self.hasher, crc32fast::Hasher::new()).finalize();
                self.crcs.push(crc);
                let mut buf = Vec::new();
                write_descriptor(&mut buf, &self.plan.entries[self.idx], crc);
                self.idx += 1;
                self.advance(&mut buf);
                self.emit(&mut out, buf.into());
            }
        }
        Ok(out)
    }

    /// Writes the central directory and end records.
    pub fn finish(&mut self) -> Result<Bytes, ZipError> {
        if !self.data_complete() {
            return Err(ZipError::Truncated);
        }
        let p = &self.plan;
        let mut b = Vec::with_capacity((p.cd_size + 98) as usize);
        for (e, crc) in p.entries.iter().zip(&self.crcs) {
            write_central(&mut b, e, *crc);
        }
        let n = p.entries.len() as u64;
        if p.zip64_end {
            let z64_offset = p.cd_offset + p.cd_size;
            put32(&mut b, 0x0606_4b50);
            put64(&mut b, 44);
            put16(&mut b, 0x032D);
            put16(&mut b, 45);
            put32(&mut b, 0);
            put32(&mut b, 0);
            put64(&mut b, n);
            put64(&mut b, n);
            put64(&mut b, p.cd_size);
            put64(&mut b, p.cd_offset);
            put32(&mut b, 0x0706_4b50);
            put32(&mut b, 0);
            put64(&mut b, z64_offset);
            put32(&mut b, 1);
        }
        put32(&mut b, 0x0605_4b50);
        put16(&mut b, 0);
        put16(&mut b, 0);
        let n16 = if n >= 0xFFFF { 0xFFFF } else { n as u16 };
        put16(&mut b, n16);
        put16(&mut b, n16);
        put32(
            &mut b,
            if p.cd_size >= U32_MAX {
                0xFFFF_FFFF
            } else {
                p.cd_size as u32
            },
        );
        put32(
            &mut b,
            if p.cd_offset >= U32_MAX {
                0xFFFF_FFFF
            } else {
                p.cd_offset as u32
            },
        );
        put16(&mut b, 0);
        self.written += b.len() as u64;
        Ok(b.into())
    }

    /// Bytes produced so far (equals [`Plan::total_len`] after `finish`).
    pub fn written(&self) -> u64 {
        self.written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(path: &str, size: u64) -> ManifestEntry {
        ManifestEntry {
            path: path.into(),
            size,
            dir: false,
            mtime: Some(1_790_624_466_000),
        }
    }

    #[test]
    fn sanitizes_paths() {
        assert_eq!(sanitize_path("a/b/c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(sanitize_path("a\\b\\c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(sanitize_path("./a//b/").unwrap(), "a/b");
        for bad in [
            "/etc/passwd",
            "../x",
            "a/../../x",
            "C:\\x",
            "c:x",
            "",
            "a/\0",
            "./",
            "a/\n",
        ] {
            assert!(sanitize_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rejects_duplicates_and_dir_data() {
        assert!(matches!(
            Plan::new(&[m("a/x", 1), m("a\\x", 2)]),
            Err(ZipError::Duplicate(_))
        ));
        let mut d = m("a/d", 3);
        d.dir = true;
        assert!(matches!(Plan::new(&[d]), Err(ZipError::DirWithData(_))));
    }

    #[test]
    fn length_is_exact_and_chunking_independent() {
        let manifest = vec![
            m("top/a.txt", 10),
            m("top/empty.bin", 0),
            m("top/sub/b.bin", 70_000),
        ];
        let mut dir = m("top/emptydir", 0);
        dir.dir = true;
        let mut manifest = manifest;
        manifest.push(dir);
        manifest.push(m("top/z.txt", 5));
        let payload: Vec<u8> = (0..70_015u32).map(|i| (i * 7) as u8).collect();
        for chunk in [1usize, 3, 4096, 1 << 20] {
            let plan = Plan::new(&manifest).unwrap();
            let total = plan.total_len();
            assert_eq!(plan.data_len(), 70_015);
            let mut z = ZipStream::new(plan);
            let mut archive: Vec<u8> = z.begin().concat();
            for c in payload.chunks(chunk) {
                for b in z.feed(Bytes::copy_from_slice(c)).unwrap() {
                    archive.extend_from_slice(&b);
                }
            }
            archive.extend_from_slice(&z.finish().unwrap());
            assert_eq!(archive.len() as u64, total);
            assert_eq!(z.written(), total);
        }
    }

    #[test]
    fn detects_truncation_and_overflow() {
        let plan = Plan::new(&[m("a", 4)]).unwrap();
        let mut z = ZipStream::new(plan.clone());
        z.begin();
        z.feed(Bytes::from_static(b"ab")).unwrap();
        assert_eq!(z.finish(), Err(ZipError::Truncated));
        let mut z = ZipStream::new(plan);
        z.begin();
        assert_eq!(
            z.feed(Bytes::from_static(b"abcde")),
            Err(ZipError::TooMuchData)
        );
    }

    #[test]
    fn empty_archive() {
        let plan = Plan::new(&[]).unwrap();
        assert_eq!(plan.total_len(), 22);
        let mut z = ZipStream::new(plan);
        assert!(z.begin().is_empty());
        assert_eq!(z.finish().unwrap().len(), 22);
    }

    #[test]
    fn zip64_layout_is_planned_for_huge_entries() {
        let plan = Plan::new(&[m("big.iso", 5 << 30), m("after.txt", 1)]).unwrap();
        // big entry: local 30+7+20, data, descriptor 24; second entry offset > 4 GiB
        let first = 30 + 7 + 20 + (5u64 << 30) + 24;
        let second = 30 + 9 + 1 + 16;
        let cd = (46 + 7 + 20) + (46 + 9 + 12);
        assert_eq!(plan.total_len(), first + second + cd + 56 + 20 + 22);
    }
}
