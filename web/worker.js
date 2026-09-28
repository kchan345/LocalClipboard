// Transfer worker: runs the LCF1 codec (lcf.wasm, compiled from Rust) off the UI
// thread. Files are read in CHUNK-sized slices, so memory use stays constant no
// matter how big the file or folder is.
"use strict";

const CHUNK = 256 * 1024; // bytes read from disk per frame
const HIGH_WATER = 4 * 1024 * 1024; // pause reading while this much is queued on the socket
const RAW_STREAK_LIMIT = 3; // stop trying lz4 on a file after this many incompressible chunks
const KIND = { RAW: 0, LZ4: 1, MANIFEST: 2, END: 3, ERROR: 4 };

let codec = null;

async function loadCodec() {
  const bytes = await (await fetch("/lcf.wasm")).arrayBuffer();
  const { instance } = await WebAssembly.instantiate(bytes, {});
  const x = instance.exports;
  const maxChunk = x.lcf_max_chunk() >>> 0;
  const cap = x.lcf_max_frame_len(maxChunk) >>> 0;
  const inPtr = x.lcf_alloc(cap) >>> 0;
  const outPtr = x.lcf_alloc(cap) >>> 0;
  // Views must be recreated after every call because memory may grow.
  const view = (ptr, len) => new Uint8Array(x.memory.buffer, ptr, len);
  return {
    maxChunk,
    encode(chunk, tryCompress) {
      view(inPtr, chunk.length).set(chunk);
      const n = x.lcf_encode(inPtr, chunk.length, outPtr, cap, tryCompress ? 1 : 0);
      if (n < 0) throw new Error("encode failed (" + n + ")");
      return view(outPtr, n).slice();
    },
    decode(frame) {
      if (frame.length > cap) throw new Error("frame too large");
      view(inPtr, frame.length).set(frame);
      const n = x.lcf_decode(inPtr, frame.length, outPtr, cap);
      if (n < 0) throw new Error("corrupt frame (" + n + ")");
      return view(outPtr, n).slice();
    },
  };
}

const codecReady = loadCodec().then(
  (c) => (codec = c),
  (e) => {
    postMessage({ type: "codecError", error: String(e && e.message ? e.message : e) });
    throw e;
  },
);

function control(kind, payload) {
  const p = payload || new Uint8Array(0);
  const buf = new Uint8Array(9 + p.length);
  const dv = new DataView(buf.buffer);
  buf[0] = kind;
  dv.setUint32(1, kind === KIND.END ? 0 : p.length, true);
  dv.setUint32(5, p.length, true);
  buf.set(p, 9);
  return buf;
}

function socketUrl(path) {
  const u = new URL(path, self.location.href);
  u.protocol = u.protocol === "https:" ? "wss:" : "ws:";
  return u.href;
}

function openSocket(path) {
  const ws = new WebSocket(socketUrl(path));
  ws.binaryType = "arraybuffer";
  const closed = new Promise((resolve) => {
    ws.addEventListener("close", (e) => resolve({ code: e.code, reason: e.reason }));
  });
  const opened = new Promise((resolve, reject) => {
    ws.addEventListener("open", () => resolve(), { once: true });
    ws.addEventListener("error", () => reject(new Error("could not open transfer socket")), { once: true });
  });
  return { ws, opened, closed };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function drain(ws) {
  while (ws.bufferedAmount > HIGH_WATER) {
    if (ws.readyState !== WebSocket.OPEN) throw new Error("transfer socket closed");
    await sleep(8);
  }
  if (ws.readyState !== WebSocket.OPEN) throw new Error("transfer socket closed");
}

// Streams one File as data frames. Reads the next slice while the current one is encoded and sent.
async function streamFile(ws, file, compress, onBytes) {
  let tryCompress = compress;
  let rawStreak = 0;
  let off = 0;
  const read = (o) => file.slice(o, Math.min(o + CHUNK, file.size)).arrayBuffer();
  let next = file.size > 0 ? read(0) : null;
  while (next) {
    const buf = new Uint8Array(await next);
    if (buf.length === 0 && off < file.size) throw new Error("file became unreadable: " + file.name);
    off += buf.length;
    next = off < file.size ? read(off) : null;
    const frame = codec.encode(buf, tryCompress);
    if (tryCompress) {
      rawStreak = frame[0] === KIND.RAW ? rawStreak + 1 : 0;
      if (rawStreak >= RAW_STREAK_LIMIT) tryCompress = false;
    }
    await drain(ws);
    ws.send(frame);
    onBytes(buf.length);
  }
  if (off !== file.size) throw new Error("file changed while sending: " + file.name);
}

async function runSend(job) {
  await codecReady;
  const { ws, opened, closed } = openSocket("/relay/" + job.token);
  await opened;
  let sent = 0;
  let lastReport = 0;
  const report = (n) => {
    sent += n;
    const now = Date.now();
    if (now - lastReport > 250) {
      lastReport = now;
      postMessage({ type: "progress", job: job.job, id: job.id, done: sent, total: job.total });
    }
  };
  try {
    if (job.error) throw new Error(job.error);
    if (job.kind === "dir") {
      const manifest = job.entries.map((e) =>
        e.dir ? { path: e.path, dir: true, mtime: e.mtime } : { path: e.path, size: e.file.size, mtime: e.mtime },
      );
      await drain(ws);
      ws.send(control(KIND.MANIFEST, new TextEncoder().encode(JSON.stringify(manifest))));
      for (const e of job.entries) {
        if (!e.dir) await streamFile(ws, e.file, job.compress && e.compress, report);
      }
    } else {
      await streamFile(ws, job.file, job.compress, report);
    }
    await drain(ws);
    ws.send(control(KIND.END));
  } catch (err) {
    if (ws.readyState === WebSocket.OPEN) {
      ws.send(control(KIND.ERROR, new TextEncoder().encode(String(err.message || err).slice(0, 500))));
      ws.close(1000);
    }
    postMessage({ type: "sendError", job: job.job, id: job.id, error: String(err.message || err) });
    return;
  }
  const c = await closed;
  if (c.code === 1000) {
    postMessage({ type: "sendDone", job: job.job, id: job.id, total: sent });
  } else {
    postMessage({ type: "sendError", job: job.job, id: job.id, error: c.reason || "transfer interrupted" });
  }
}

// Receives a small file over /pull/{id} still compressed and decodes it here.
async function runPull(job) {
  await codecReady;
  const { ws, opened, closed } = openSocket("/pull/" + encodeURIComponent(job.id));
  const parts = [];
  let received = 0;
  let finished = false;
  let failure = null;
  let lastReport = 0;
  ws.addEventListener("message", (ev) => {
    if (finished || failure) return;
    try {
      const frame = new Uint8Array(ev.data);
      const kind = frame[0];
      if (kind === KIND.RAW || kind === KIND.LZ4) {
        const plain = kind === KIND.RAW ? frame.subarray(9) : codec.decode(frame);
        received += plain.length;
        if (received > job.size) throw new Error("received more data than announced");
        parts.push(kind === KIND.RAW ? plain.slice() : plain);
        const now = Date.now();
        if (now - lastReport > 250) {
          lastReport = now;
          postMessage({ type: "progress", job: job.job, id: job.id, done: received, total: job.size });
        }
      } else if (kind === KIND.END) {
        finished = true;
      } else if (kind === KIND.ERROR) {
        failure = new TextDecoder().decode(frame.subarray(9)) || "transfer failed";
      } else {
        throw new Error("unexpected frame");
      }
    } catch (e) {
      failure = String(e.message || e);
      ws.close();
    }
  });
  try {
    await opened;
  } catch (e) {
    postMessage({ type: "pullError", job: job.job, id: job.id, error: String(e.message || e) });
    return;
  }
  const c = await closed;
  if (finished && !failure && received === job.size) {
    const blob = new Blob(parts, { type: job.mime || "application/octet-stream" });
    postMessage({ type: "pullDone", job: job.job, id: job.id, blob });
  } else {
    postMessage({
      type: "pullError",
      job: job.job,
      id: job.id,
      error: failure || c.reason || "transfer interrupted",
    });
  }
}

self.onmessage = (ev) => {
  const m = ev.data;
  const run = m.type === "send" ? runSend : m.type === "pull" ? runPull : null;
  if (!run) return;
  run(m).catch((e) => {
    postMessage({
      type: m.type === "send" ? "sendError" : "pullError",
      job: m.job,
      id: m.id,
      error: String(e && e.message ? e.message : e),
    });
  });
};
