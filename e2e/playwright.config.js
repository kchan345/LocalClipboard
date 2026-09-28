// @ts-check
const { defineConfig } = require("@playwright/test");

// The workflow starts the binary and passes its URLs:
//   LC_URL       – a LAN address (exercises lz4 in the sender worker and WASM decode on /pull)
//   LC_LOCAL_URL – the loopback address (exercises the plain streamed /file download)
module.exports = defineConfig({
  testDir: "./tests",
  timeout: 60_000,
  retries: 0,
  workers: 1,
  reporter: [["list"], ["html", { open: "never" }]],
  use: {
    browserName: "chromium",
    acceptDownloads: true,
    trace: "retain-on-failure",
  },
});
