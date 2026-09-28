//! C-ABI exports of the LCF1 codec for `wasm32-unknown-unknown`.
//!
//! The module is deliberately built without wasm-bindgen: it has no imports and
//! a handful of pointer/length exports, so the browser loads it with plain
//! `WebAssembly.instantiate` and no generated glue or extra tooling is needed.
//!
//! Memory protocol (see `web/worker.js`): the JS side allocates one input and one
//! output buffer with [`lcf_alloc`] sized for the largest chunk, copies each
//! chunk into the input buffer, calls [`lcf_encode`] / [`lcf_decode`] and copies
//! the result out. Buffers are reused for the whole transfer, so a transfer of
//! any size uses a constant amount of WASM memory.

use lcf::{decode_into, encode_data_into, max_frame_len, parse, MAX_CHUNK};

/// Error codes returned (negated) by the exports.
pub const ERR_ARGS: isize = -1;
pub const ERR_FRAME: isize = -2;
pub const ERR_CORRUPT: isize = -3;

/// Allocates `len` bytes inside WASM linear memory and returns the pointer.
#[no_mangle]
pub extern "C" fn lcf_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len.max(1));
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

/// Frees a buffer returned by [`lcf_alloc`].
///
/// # Safety
/// `ptr` must come from `lcf_alloc(len)` with the same `len` and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn lcf_free(ptr: *mut u8, len: usize) {
    drop(Vec::from_raw_parts(ptr, 0, len.max(1)));
}

/// Largest accepted chunk size.
#[no_mangle]
pub extern "C" fn lcf_max_chunk() -> usize {
    MAX_CHUNK
}

/// Worst-case frame size for an `n`-byte chunk.
#[no_mangle]
pub extern "C" fn lcf_max_frame_len(n: usize) -> usize {
    max_frame_len(n)
}

/// Encodes `input[..n]` as one data frame into `out[..cap]`.
/// Returns the frame length or a negative error code.
///
/// # Safety
/// `input` must be valid for `n` bytes and `out` for `cap` bytes; they must not overlap.
#[no_mangle]
pub unsafe extern "C" fn lcf_encode(
    input: *const u8,
    n: usize,
    out: *mut u8,
    cap: usize,
    try_compress: u32,
) -> isize {
    if input.is_null() || out.is_null() {
        return ERR_ARGS;
    }
    let input = std::slice::from_raw_parts(input, n);
    let out = std::slice::from_raw_parts_mut(out, cap);
    match encode_data_into(input, try_compress != 0, out) {
        Ok(len) => len as isize,
        Err(_) => ERR_ARGS,
    }
}

/// Returns the frame kind (0..=4) of the frame in `input[..n]`, or a negative error.
///
/// # Safety
/// `input` must be valid for `n` bytes.
#[no_mangle]
pub unsafe extern "C" fn lcf_frame_kind(input: *const u8, n: usize) -> isize {
    if input.is_null() {
        return ERR_ARGS;
    }
    match parse(std::slice::from_raw_parts(input, n)) {
        Ok(f) => f.kind as u8 as isize,
        Err(_) => ERR_FRAME,
    }
}

/// Decodes the data frame in `input[..n]` into `out[..cap]`.
/// Returns the number of decoded bytes or a negative error code.
///
/// # Safety
/// `input` must be valid for `n` bytes and `out` for `cap` bytes; they must not overlap.
#[no_mangle]
pub unsafe extern "C" fn lcf_decode(input: *const u8, n: usize, out: *mut u8, cap: usize) -> isize {
    if input.is_null() || out.is_null() {
        return ERR_ARGS;
    }
    let frame = match parse(std::slice::from_raw_parts(input, n)) {
        Ok(f) if f.kind.is_data() => f,
        Ok(_) => return ERR_FRAME,
        Err(_) => return ERR_FRAME,
    };
    let out = std::slice::from_raw_parts_mut(out, cap);
    match decode_into(&frame, out) {
        Ok(len) => len as isize,
        Err(_) => ERR_CORRUPT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_roundtrip() {
        let data: Vec<u8> = b"abcdefgh".iter().cycle().take(100_000).copied().collect();
        let cap = lcf_max_frame_len(data.len());
        let mut frame = vec![0u8; cap];
        let n = unsafe { lcf_encode(data.as_ptr(), data.len(), frame.as_mut_ptr(), cap, 1) };
        assert!(n > 0 && (n as usize) < data.len());
        assert_eq!(unsafe { lcf_frame_kind(frame.as_ptr(), n as usize) }, 1);
        let mut out = vec![0u8; lcf_max_chunk()];
        let m = unsafe { lcf_decode(frame.as_ptr(), n as usize, out.as_mut_ptr(), out.len()) };
        assert_eq!(m as usize, data.len());
        assert_eq!(&out[..m as usize], &data[..]);
        let p = lcf_alloc(16);
        unsafe { lcf_free(p, 16) };
    }
}
