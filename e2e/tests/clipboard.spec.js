// @ts-check
const { test, expect } = require("@playwright/test");
const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const LAN = process.env.LC_URL || "http://127.0.0.1:8080";
const LOCAL = process.env.LC_LOCAL_URL || "http://127.0.0.1:8080";

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "lc-e2e-"));

function textData(n) {
  const line = "LocalClipboard streams attachments straight from the sharing device. ";
  return Buffer.from(line.repeat(Math.ceil(n / line.length)).slice(0, n));
}

function randomData(n) {
  return require("node:crypto").randomBytes(n);
}

async function openPage(browser, url) {
  const ctx = await browser.newContext({ acceptDownloads: true });
  const page = await ctx.newPage();
  page.on("pageerror", (e) => console.error("[pageerror]", e));
  await page.goto(url);
  await expect(page.getByTestId("status")).toContainText("Connected");
  return page;
}

async function sendFiles(page, selector, files) {
  await page.locator(selector).setInputFiles(files);
  await expect(page.locator("#chips .chip").first()).toBeVisible();
  await page.locator("#sendBtn").click();
}

async function downloadFrom(page, name) {
  const box = page.getByTestId("attachment").filter({ hasText: name });
  await expect(box).toBeVisible();
  const [dl] = await Promise.all([page.waitForEvent("download"), box.getByRole("button", { name: "Download" }).click()]);
  const out = path.join(tmp, "dl-" + Date.now() + "-" + dl.suggestedFilename());
  await dl.saveAs(out);
  expect(await dl.failure()).toBeNull();
  return { file: out, name: dl.suggestedFilename() };
}

test("text is shared between devices", async ({ browser }) => {
  const a = await openPage(browser, LAN);
  const b = await openPage(browser, LOCAL);
  await a.locator("#messageInput").fill("hello from A — שלום");
  await a.locator("#messageInput").press("Enter");
  const onB = b.locator(".msg").filter({ hasText: "hello from A — שלום" });
  await expect(onB).toBeVisible();
  await expect(onB.locator(".msg-head")).toContainText("From ");
  await expect(a.locator(".msg.mine").filter({ hasText: "hello from A" })).toBeVisible();
});

test("files stream from the sender (lz4 over WASM) and download intact", async ({ browser }) => {
  const sender = await openPage(browser, LAN);
  const lanReceiver = await openPage(browser, LAN);
  const localReceiver = await openPage(browser, LOCAL);

  const small = path.join(tmp, "notes.txt");
  const big = path.join(tmp, "blob.bin");
  fs.writeFileSync(small, textData(3 * 1024 * 1024 + 17));
  fs.writeFileSync(big, Buffer.concat([randomData(2 * 1024 * 1024), textData(2 * 1024 * 1024)]));
  await sendFiles(sender, "#fileInput", [small, big]);

  // LAN receiver: small files go over /pull and are decoded by the WASM codec in the worker.
  for (const [p, recv] of [
    [small, lanReceiver],
    [big, lanReceiver],
    [small, localReceiver],
    [big, localReceiver],
  ]) {
    const got = await downloadFrom(/** @type {any} */ (recv), path.basename(/** @type {string} */ (p)));
    expect(got.name).toBe(path.basename(/** @type {string} */ (p)));
    expect(fs.readFileSync(got.file).equals(fs.readFileSync(/** @type {string} */ (p)))).toBe(true);
  }
});

test("folders download as a zip with their structure", async ({ browser }) => {
  const sender = await openPage(browser, LAN);
  const receiver = await openPage(browser, LOCAL);

  const root = path.join(tmp, "album");
  fs.mkdirSync(path.join(root, "nested", "deeper"), { recursive: true });
  const files = {
    "a.txt": textData(500_000),
    "nested/b.bin": randomData(300_000),
    "nested/deeper/c.txt": textData(10),
    "zero.txt": Buffer.alloc(0),
  };
  for (const [rel, data] of Object.entries(files)) fs.writeFileSync(path.join(root, rel), data);

  await sendFiles(sender, "#folderInput", root);
  const got = await downloadFrom(receiver, "album");
  expect(got.name).toBe("album.zip");

  const listing = execFileSync("unzip", ["-Z1", got.file], { encoding: "utf8" }).trim().split("\n");
  for (const [rel, data] of Object.entries(files)) {
    expect(listing).toContain("album/" + rel);
    const content = execFileSync("unzip", ["-p", got.file, "album/" + rel], { maxBuffer: 16 << 20 });
    expect(content.equals(data)).toBe(true);
  }
  execFileSync("unzip", ["-tq", got.file]);
});

test("attachments become unavailable when the sender leaves", async ({ browser }) => {
  const sender = await openPage(browser, LAN);
  const receiver = await openPage(browser, LOCAL);
  const f = path.join(tmp, "leaving.txt");
  fs.writeFileSync(f, textData(1000));
  await sendFiles(sender, "#fileInput", [f]);
  const box = receiver.getByTestId("attachment").filter({ hasText: "leaving.txt" });
  await expect(box).toBeVisible();
  await sender.context().close();
  await expect(box.getByRole("button")).toHaveText("Unavailable");
  await expect(box).toContainText("sender offline");
  const status = await receiver.evaluate(async () => {
    const id = document.querySelector('[data-testid="attachment"]')?.closest(".msg")?.getAttribute("data-id");
    return (await fetch("/file/" + id)).status;
  });
  expect(status).toBe(410);
});
