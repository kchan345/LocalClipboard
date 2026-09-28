// Cross-checks the browser codec (web/worker.js + web/lcf.wasm) against the
// Rust `lcf` crate. Driven by tests/wasm_compat.rs:
//
//   node tests/js/wasm_compat.mjs <repo root> <rust-frames.bin> <js-frames.bin>
//
// Both files are sequences of records `u32 LE length | bytes`, alternating
// original data and its encoded LCF1 frame. This script decodes every frame
// the Rust side produced using the real worker code, then writes frames
// encoded by the worker for the Rust side to decode.
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import vm from "node:vm";

const [root, rustIn, jsOut] = process.argv.slice(2);
const wasm = readFileSync(join(root, "web", "lcf.wasm"));
const source = readFileSync(join(root, "web", "worker.js"), "utf8");

const context = vm.createContext({
  WebAssembly,
  TextEncoder,
  TextDecoder,
  URL,
  Blob,
  setTimeout,
  console,
  fetch: async () => ({ arrayBuffer: async () => wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.length) }),
  postMessage: (m) => {
    throw new Error("unexpected postMessage " + JSON.stringify(m));
  },
});
context.self = context;
vm.runInContext(
  source + "\n;globalThis.__lcf = { codecReady, control, KIND, get codec() { return codec; } };",
  context,
  { filename: "worker.js" },
);
const lcf = context.__lcf;
await lcf.codecReady;
const codec = lcf.codec;

function readRecords(path) {
  const buf = readFileSync(path);
  const out = [];
  let off = 0;
  while (off < buf.length) {
    const n = buf.readUInt32LE(off);
    out.push(new Uint8Array(buf.subarray(off + 4, off + 4 + n)));
    off += 4 + n;
  }
  return out;
}

function decodeFrame(frame) {
  return frame[0] === lcf.KIND.RAW ? frame.subarray(9) : codec.decode(frame);
}

function equal(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

let failures = 0;
const recs = readRecords(rustIn);
for (let i = 0; i < recs.length; i += 2) {
  const [orig, frame] = [recs[i], recs[i + 1]];
  if (!equal(decodeFrame(frame), orig)) {
    console.error(`rust frame #${i / 2} (kind ${frame[0]}, ${orig.length} bytes) decoded incorrectly`);
    failures++;
  }
}

// Fixtures encoded by the worker.
function lcg(n, seed) {
  const a = new Uint8Array(n);
  let s = seed >>> 0;
  for (let i = 0; i < n; i++) {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    a[i] = s >>> 24;
  }
  return a;
}
const enc = new TextEncoder();
const fixtures = [
  new Uint8Array(0),
  new Uint8Array([42]),
  enc.encode("x".repeat(63)),
  enc.encode("hello ".repeat(12)),
  enc.encode("The quick brown fox jumps over the lazy dog. ".repeat(6000)),
  new Uint8Array(codec.maxChunk),
  lcg(256 * 1024, 1),
  Uint8Array.from({ length: codec.maxChunk }, (_, i) => (i * 7) % 251),
];
const out = [];
const push = (b) => {
  const len = Buffer.alloc(4);
  len.writeUInt32LE(b.length);
  out.push(len, Buffer.from(b.buffer, b.byteOffset, b.length));
};
let compressed = 0;
for (const f of fixtures) {
  for (const tryCompress of [true, false]) {
    const frame = codec.encode(f, tryCompress);
    if (!tryCompress && frame[0] !== lcf.KIND.RAW) {
      console.error("encode without compression produced a non-raw frame");
      failures++;
    }
    if (frame[0] === lcf.KIND.LZ4) compressed++;
    if (!equal(decodeFrame(frame), f)) {
      console.error(`worker round trip failed for ${f.length} bytes`);
      failures++;
    }
    push(f);
    push(frame);
  }
}
if (compressed < 3) {
  console.error("expected compressible fixtures to produce lz4 frames");
  failures++;
}

// Control frames built by the worker must match the Rust layout.
const end = lcf.control(lcf.KIND.END);
const err = lcf.control(lcf.KIND.ERROR, enc.encode("boom"));
push(new Uint8Array(0));
push(end);
push(enc.encode("boom"));
push(err);

try {
  codec.decode(new Uint8Array([1, 255, 255, 255, 127, 4, 0, 0, 0, 1, 2, 3, 4]));
  console.error("decoding a bomb header should fail");
  failures++;
} catch {
  // expected
}

writeFileSync(jsOut, Buffer.concat(out));
if (failures) {
  console.error(`${failures} failure(s)`);
  process.exit(1);
}
console.log(`ok: ${recs.length / 2} rust frames decoded, ${fixtures.length * 2} js frames encoded`);
