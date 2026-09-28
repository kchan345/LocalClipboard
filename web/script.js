// Local Clipboard UI. Messages live only in this page; attachments stay on the
// device that shared them and are streamed on demand by worker.js.
"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const el = {
    status: $("status"),
    messages: $("messages"),
    input: $("messageInput"),
    send: $("sendBtn"),
    attachFile: $("attachFileBtn"),
    attachFolder: $("attachFolderBtn"),
    fileInput: $("fileInput"),
    folderInput: $("folderInput"),
    chips: $("chips"),
    overlay: $("dropOverlay"),
    interval: $("intervalSelect"),
    countdown: $("countdown"),
    pause: $("pauseBtn"),
    clear: $("clearBtn"),
    qrToggle: $("qrToggle"),
    qrBody: $("qrBody"),
    qrImage: $("qrImage"),
    version: $("version"),
    banner: $("updateBanner"),
    latest: $("latestVersion"),
    updateLink: $("updateLink"),
    dismissUpdate: $("dismissUpdate"),
  };

  const REPO = "kchan345/LocalClipboard";
  const COMPRESSED_EXT = new Set(
    ("jpg jpeg png gif webp heic heif avif jxl mp4 m4v mov mkv webm avi mp3 m4a aac ogg opus flac " +
      "zip gz tgz bz2 xz zst 7z rar br lz4 jar apk ipa dmg pdf docx xlsx pptx odt ods epub woff woff2")
      .split(" "),
  );
  const loopback = /^(localhost|127\.|\[::1\])/.test(location.hostname);

  let ws = null;
  let hello = { transferCompression: true, browserDecodeMaxBytes: 64 << 20 };
  let config = { intervalMin: 10, paused: false, nextClearTime: null };
  let queued = []; // attachments waiting to be sent
  const owned = new Map(); // id -> attachment shared from this page
  const awaitingEcho = new Map(); // ref -> attachment
  const sentRefs = new Set(); // refs sent from this page, to label echoes as "You"
  const rendered = new Map(); // id -> { card, button, info, bar }
  let refSeq = 0;
  let jobSeq = 0;
  const jobs = new Map(); // job -> handlers

  // ---------- helpers ----------
  function fmtBytes(n) {
    if (n < 1024) return n + " B";
    const u = ["KB", "MB", "GB", "TB"];
    let i = -1;
    do {
      n /= 1024;
      i++;
    } while (n >= 1024 && i < u.length - 1);
    return (n >= 10 ? n.toFixed(0) : n.toFixed(1)) + " " + u[i];
  }

  function describe(a) {
    return a.kind === "dir"
      ? a.count + (a.count === 1 ? " file" : " files") + " · " + fmtBytes(a.size)
      : fmtBytes(a.size);
  }

  function toast(text) {
    const t = document.createElement("div");
    t.className = "toast";
    t.textContent = text;
    document.body.appendChild(t);
    setTimeout(() => t.remove(), 2200);
  }

  function isCompressedAlready(name, type) {
    const ext = (name.split(".").pop() || "").toLowerCase();
    if (COMPRESSED_EXT.has(ext)) return true;
    return /^(image\/(jpeg|png|gif|webp|heic|avif)|video\/|audio\/(mpeg|mp4|aac|ogg|opus|flac))/.test(type || "");
  }

  function saveBlob(blob, name) {
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = name;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60000);
  }

  function nativeDownload(id) {
    const a = document.createElement("a");
    a.href = "/file/" + encodeURIComponent(id);
    a.download = "";
    document.body.appendChild(a);
    a.click();
    a.remove();
  }

  async function copyText(text) {
    try {
      if (navigator.clipboard && window.isSecureContext) {
        await navigator.clipboard.writeText(text);
        return true;
      }
    } catch (_) {
      /* fall through */
    }
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch (_) {
      ok = false;
    }
    ta.remove();
    return ok;
  }

  // ---------- worker ----------
  const worker = new Worker("worker.js");
  worker.onmessage = (ev) => {
    const m = ev.data;
    if (m.type === "codecError") {
      console.error("transfer codec failed to load:", m.error);
      return;
    }
    const h = jobs.get(m.job);
    if (!h) return;
    if (m.type === "progress") h.progress && h.progress(m.done, m.total);
    else {
      jobs.delete(m.job);
      if (m.type === "sendDone" || m.type === "pullDone") h.done && h.done(m);
      else h.error && h.error(m.error);
    }
  };

  function startJob(msg, handlers) {
    const job = ++jobSeq;
    jobs.set(job, handlers || {});
    worker.postMessage(Object.assign({ job }, msg));
    return job;
  }

  function serveRequest(req) {
    const a = owned.get(req.id);
    const base = { type: "send", token: req.token, id: req.id, mode: req.mode };
    if (!a) {
      startJob(Object.assign(base, { error: "attachment is no longer shared" }));
      return;
    }
    // Compression pays off on the sender -> server leg, except when both run on this machine
    // and the server decodes it anyway ("plain" mode).
    const compress = hello.transferCompression && !(loopback && req.mode === "plain");
    const view = rendered.get(req.id);
    const handlers = {
      progress: (done, total) => view && setProgress(view, done / Math.max(total, 1), "sending"),
      done: () => view && setProgress(view, null),
      error: (e) => {
        if (view) setProgress(view, null);
        console.warn("send failed:", e);
      },
    };
    if (a.kind === "dir") {
      startJob(Object.assign(base, { kind: "dir", entries: a.entries, total: a.size, compress }), handlers);
    } else {
      startJob(
        Object.assign(base, {
          kind: "file",
          file: a.file,
          total: a.size,
          compress: compress && !isCompressedAlready(a.name, a.type),
        }),
        handlers,
      );
    }
  }

  // ---------- rendering ----------
  function setProgress(view, frac, label) {
    if (frac === null) {
      view.bar.parentElement.hidden = true;
      view.info.textContent = view.baseInfo;
      return;
    }
    view.bar.parentElement.hidden = false;
    view.bar.style.width = Math.round(frac * 100) + "%";
    view.info.textContent = view.baseInfo + " · " + label + " " + Math.round(frac * 100) + "%";
  }

  function clearEmpty() {
    const e = el.messages.querySelector(".empty");
    if (e) e.remove();
  }

  function showEmpty() {
    el.messages.innerHTML = '<p class="empty">No messages yet. Type something, or drop files or folders here.</p>';
  }

  function renderMessage(m, mine) {
    clearEmpty();
    const card = document.createElement("article");
    card.className = "msg" + (mine ? " mine" : "");
    card.dataset.id = m.id;

    const head = document.createElement("div");
    head.className = "msg-head";
    const who = document.createElement("span");
    who.textContent = mine ? "You" : "From " + (m.senderIp || "unknown");
    const when = document.createElement("span");
    when.textContent = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    head.append(who, when);
    card.appendChild(head);

    if (m.file) {
      const f = m.file;
      const box = document.createElement("div");
      box.className = "file";
      box.dataset.testid = "attachment";
      const icon = document.createElement("span");
      icon.textContent = f.kind === "dir" ? "📁" : "📄";
      const grow = document.createElement("div");
      grow.className = "grow";
      const name = document.createElement("div");
      name.className = "fname";
      name.textContent = f.kind === "dir" ? f.name + "/" : f.name;
      const info = document.createElement("div");
      info.className = "finfo";
      const baseInfo = describe(f) + (f.kind === "dir" ? " · downloads as .zip" : "");
      info.textContent = baseInfo;
      const prog = document.createElement("div");
      prog.className = "progress";
      prog.hidden = true;
      const bar = document.createElement("div");
      prog.appendChild(bar);
      grow.append(name, info, prog);
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "download";
      btn.textContent = "Download";
      box.append(icon, grow, btn);
      card.appendChild(box);
      const view = { card, box, button: btn, info, bar, baseInfo, file: f, mine };
      rendered.set(f.id, view);
      btn.addEventListener("click", () => download(view));
    }

    if (m.text) {
      const p = document.createElement("div");
      p.className = "msg-text";
      p.dir = "auto";
      p.textContent = m.text;
      card.appendChild(p);
      const actions = document.createElement("div");
      actions.className = "msg-actions";
      const copy = document.createElement("button");
      copy.type = "button";
      copy.textContent = "Copy";
      copy.addEventListener("click", async () => {
        const ok = await copyText(m.text);
        copy.textContent = ok ? "Copied!" : "Copy failed";
        setTimeout(() => (copy.textContent = "Copy"), 1500);
      });
      actions.appendChild(copy);
      card.appendChild(actions);
    }

    el.messages.appendChild(card);
    card.scrollIntoView({ block: "end", behavior: "smooth" });
  }

  function markUnavailable(id) {
    const v = rendered.get(id);
    if (!v || v.mine) return;
    v.box.classList.add("gone");
    v.button.disabled = true;
    v.button.textContent = "Unavailable";
    v.baseInfo = describe(v.file) + " · sender offline";
    v.info.textContent = v.baseInfo;
    v.bar.parentElement.hidden = true;
  }

  function download(view) {
    const f = view.file;
    const own = owned.get(f.id);
    if (own && own.kind === "file") {
      saveBlob(own.file, own.name);
      return;
    }
    // Small files from another device: fetch them still lz4-compressed and decode in the
    // worker (WASM), saving LAN bandwidth on the server -> receiver leg as well.
    const pull =
      !own && f.kind === "file" && f.size > 0 && f.size <= hello.browserDecodeMaxBytes && !loopback && hello.transferCompression;
    if (!pull) {
      nativeDownload(f.id);
      return;
    }
    view.button.disabled = true;
    startJob(
      { type: "pull", id: f.id, size: f.size, mime: f.type, name: f.name },
      {
        progress: (done, total) => setProgress(view, done / Math.max(total, 1), "receiving"),
        done: (m) => {
          view.button.disabled = false;
          setProgress(view, null);
          saveBlob(m.blob, f.name);
        },
        error: (e) => {
          view.button.disabled = false;
          setProgress(view, null);
          if (/offline|not found/i.test(e)) {
            toast(e);
          } else {
            // Fall back to a normal streamed download.
            nativeDownload(f.id);
          }
        },
      },
    );
  }

  // ---------- auto-clear bar ----------
  let tick = null;
  function renderConfig() {
    const v = String(config.intervalMin);
    if (![...el.interval.options].some((o) => o.value === v)) {
      const o = document.createElement("option");
      o.value = v;
      o.textContent = v + " min";
      el.interval.appendChild(o);
    }
    el.interval.value = v;
    el.pause.hidden = config.intervalMin === 0;
    el.pause.textContent = config.paused ? "▶" : "⏸";
    el.pause.title = config.paused ? "Resume timer" : "Pause timer";
    clearInterval(tick);
    const update = () => {
      if (config.intervalMin === 0) {
        el.countdown.hidden = true;
        return;
      }
      el.countdown.hidden = false;
      el.countdown.classList.toggle("paused", config.paused);
      if (config.paused || !config.nextClearTime) {
        el.countdown.textContent = "Paused";
        return;
      }
      const left = Math.max(0, Math.round((Date.parse(config.nextClearTime) - Date.now()) / 1000));
      const mm = Math.floor(left / 60);
      const ss = String(left % 60).padStart(2, "0");
      el.countdown.textContent = "Clears in " + mm + ":" + ss;
    };
    update();
    tick = setInterval(update, 1000);
  }

  function post(path, body) {
    return fetch(path, {
      method: "POST",
      headers: body ? { "Content-Type": "application/json" } : {},
      body: body ? JSON.stringify(body) : undefined,
    }).catch(() => toast("Server unreachable"));
  }

  el.interval.addEventListener("change", () => post("/set-interval", { interval: Number(el.interval.value) }));
  el.pause.addEventListener("click", () => post("/toggle-pause"));
  el.clear.addEventListener("click", () => {
    if (confirm("Clear all messages on every connected device?")) post("/clear");
  });

  // ---------- composing ----------
  function renderChips() {
    el.chips.innerHTML = "";
    el.chips.hidden = queued.length === 0;
    queued.forEach((a, i) => {
      const c = document.createElement("span");
      c.className = "chip";
      const s = document.createElement("span");
      s.textContent = (a.kind === "dir" ? "📁 " : "📄 ") + a.name + " · " + describe(a);
      const x = document.createElement("button");
      x.type = "button";
      x.textContent = "✕";
      x.title = "Remove";
      x.addEventListener("click", () => {
        queued.splice(i, 1);
        renderChips();
      });
      c.append(s, x);
      el.chips.appendChild(c);
    });
  }

  function fileAttachment(file) {
    return { kind: "file", name: file.name, size: file.size, type: file.type || "", file, count: 1 };
  }

  function dirAttachment(name, entries) {
    entries.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
    let size = 0;
    let count = 0;
    for (const e of entries) {
      if (!e.dir) {
        size += e.file.size;
        count++;
        e.compress = !isCompressedAlready(e.file.name, e.file.type);
      }
    }
    return { kind: "dir", name, size, count, entries, type: "" };
  }

  function addFiles(list) {
    for (const f of list) queued.push(fileAttachment(f));
    renderChips();
  }

  // <input webkitdirectory>: files carry "top/sub/name" in webkitRelativePath.
  function addFolderFiles(list) {
    const groups = new Map();
    for (const f of list) {
      const rel = f.webkitRelativePath || f.name;
      const top = rel.split("/")[0];
      if (!groups.has(top)) groups.set(top, []);
      groups.get(top).push({ path: rel, file: f, mtime: f.lastModified });
    }
    for (const [name, entries] of groups) queued.push(dirAttachment(name, entries));
    renderChips();
  }

  function send() {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const text = el.input.value;
    const hasText = text.trim().length > 0;
    if (!hasText && queued.length === 0) return;
    const items = queued.length ? queued : [null];
    items.forEach((a, i) => {
      const ref = "r" + ++refSeq + "-" + Math.random().toString(36).slice(2, 8);
      const msg = { ref, text: i === 0 && hasText ? text : "" };
      sentRefs.add(ref);
      if (a) {
        msg.file = { name: a.name, size: a.size, type: a.type, kind: a.kind, count: a.count };
        awaitingEcho.set(ref, a);
      }
      ws.send(JSON.stringify(msg));
    });
    queued = [];
    renderChips();
    el.input.value = "";
    el.input.focus();
  }

  el.send.addEventListener("click", send);
  el.input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      send();
    }
  });
  el.attachFile.addEventListener("click", () => el.fileInput.click());
  el.attachFolder.addEventListener("click", () => el.folderInput.click());
  el.fileInput.addEventListener("change", () => {
    addFiles([...el.fileInput.files]);
    el.fileInput.value = "";
  });
  el.folderInput.addEventListener("change", () => {
    addFolderFiles([...el.folderInput.files]);
    el.folderInput.value = "";
  });
  if (!("webkitdirectory" in el.folderInput)) el.attachFolder.hidden = true;

  // ---------- drag and drop (files and folders) ----------
  let dragDepth = 0;
  const hasFiles = (e) => e.dataTransfer && [...e.dataTransfer.types].includes("Files");
  document.addEventListener("dragenter", (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    dragDepth++;
    el.overlay.hidden = false;
  });
  document.addEventListener("dragover", (e) => {
    if (hasFiles(e)) e.preventDefault();
  });
  document.addEventListener("dragleave", () => {
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) el.overlay.hidden = true;
  });

  const entryFile = (entry) => new Promise((res, rej) => entry.file(res, rej));
  function readAll(reader) {
    return new Promise((resolve, reject) => {
      const out = [];
      const step = () =>
        reader.readEntries((batch) => {
          if (batch.length === 0) resolve(out);
          else {
            out.push(...batch);
            step();
          }
        }, reject);
      step();
    });
  }

  async function walk(dirEntry, prefix, out) {
    const children = await readAll(dirEntry.createReader());
    if (children.length === 0) out.push({ path: prefix, dir: true, mtime: null });
    for (const c of children) {
      const path = prefix + "/" + c.name;
      if (c.isDirectory) await walk(c, path, out);
      else if (c.isFile) {
        const f = await entryFile(c);
        out.push({ path, file: f, mtime: f.lastModified });
      }
    }
  }

  document.addEventListener("drop", async (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    dragDepth = 0;
    el.overlay.hidden = true;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    // Entries must be captured synchronously; the DataTransfer is emptied after this tick.
    const items = [...(e.dataTransfer.items || [])];
    const entries = items.map((it) => (it.webkitGetAsEntry ? it.webkitGetAsEntry() : null));
    const plainFiles = [...e.dataTransfer.files];
    if (!entries.some(Boolean)) {
      addFiles(plainFiles);
      return;
    }
    for (let i = 0; i < entries.length; i++) {
      const en = entries[i];
      try {
        if (en && en.isDirectory) {
          const out = [];
          await walk(en, en.name, out);
          queued.push(dirAttachment(en.name, out));
        } else if (en && en.isFile) {
          queued.push(fileAttachment(await entryFile(en)));
        } else if (plainFiles[i]) {
          queued.push(fileAttachment(plainFiles[i]));
        }
      } catch (err) {
        toast("Could not read " + (en ? en.name : "item"));
      }
    }
    renderChips();
  });

  // ---------- connection ----------
  function setOnline(on) {
    el.input.disabled = !on;
    el.send.disabled = !on;
    el.attachFile.disabled = !on;
    el.attachFolder.disabled = !on;
    if (!on) {
      el.status.textContent = "Disconnected. Reconnecting…";
      el.status.className = "pill offline";
    }
  }

  function onMessage(ev) {
    let m;
    try {
      m = JSON.parse(ev.data);
    } catch (_) {
      return;
    }
    switch (m.type) {
      case "hello":
        hello = m;
        el.version.textContent = "v" + m.version;
        checkForUpdate(m.version);
        return;
      case "config":
        config = m.config;
        renderConfig();
        return;
      case "clients":
        el.status.textContent = "🟢 Connected · " + m.count + (m.count === 1 ? " device" : " devices");
        el.status.className = "pill online";
        return;
      case "clear":
        showEmpty();
        rendered.clear();
        owned.clear();
        return;
      case "unavailable":
        (m.ids || []).forEach(markUnavailable);
        return;
      case "fileRequest":
        serveRequest(m);
        return;
      default:
        break;
    }
    if (!m.id) return;
    let mine = false;
    if (m.ref && awaitingEcho.has(m.ref)) {
      mine = true;
      const a = awaitingEcho.get(m.ref);
      awaitingEcho.delete(m.ref);
      if (m.file) owned.set(m.file.id, a);
    } else if (m.ref && sentRefs.has(m.ref)) {
      mine = true;
      sentRefs.delete(m.ref);
    }
    renderMessage(m, mine);
  }

  function connect() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(proto + "//" + location.host + "/ws");
    const sock = ws;
    sock.onopen = () => setOnline(true);
    sock.onmessage = onMessage;
    sock.onclose = () => {
      if (ws !== sock) return;
      setOnline(false);
      // The server forgets our attachments when we disconnect; others' may be gone too.
      for (const id of rendered.keys()) markUnavailable(id);
      owned.clear();
      for (const v of rendered.values()) {
        if (v.mine && v.file.kind === "dir") {
          v.button.disabled = true;
          v.info.textContent = describe(v.file) + " · no longer shared";
        }
      }
      setTimeout(connect, 2000);
    };
  }

  // ---------- QR + update check ----------
  el.qrToggle.addEventListener("click", () => {
    const show = el.qrBody.hidden;
    el.qrBody.hidden = !show;
    el.qrToggle.textContent = show ? "Hide QR code" : "Show QR code";
    el.qrToggle.setAttribute("aria-expanded", String(show));
    if (show && !el.qrImage.src) el.qrImage.src = "/qr";
  });

  function newer(a, b) {
    const pa = a.replace(/^v/, "").split(/[.-]/).map((x) => parseInt(x, 10) || 0);
    const pb = b.replace(/^v/, "").split(/[.-]/).map((x) => parseInt(x, 10) || 0);
    for (let i = 0; i < 3; i++) {
      if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) > (pb[i] || 0);
    }
    return false;
  }

  let checkedUpdate = false;
  async function checkForUpdate(current) {
    if (checkedUpdate || !current || current === "dev") return;
    checkedUpdate = true;
    try {
      const dismissed = localStorage.getItem("lc-dismissed-version");
      const r = await fetch("https://api.github.com/repos/" + REPO + "/releases/latest", {
        headers: { Accept: "application/vnd.github+json" },
      });
      if (!r.ok) return;
      const rel = await r.json();
      const tag = rel.tag_name || "";
      if (tag && newer(tag, current) && dismissed !== tag) {
        el.latest.textContent = tag;
        el.updateLink.href = rel.html_url || "https://github.com/" + REPO + "/releases/latest";
        el.banner.hidden = false;
        el.dismissUpdate.onclick = () => {
          el.banner.hidden = true;
          localStorage.setItem("lc-dismissed-version", tag);
        };
      }
    } catch (_) {
      /* offline LAN: ignore */
    }
  }

  connect();
})();
