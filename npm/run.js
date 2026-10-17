"use strict";
// Run a vendored binary with this process's arguments and exit status. If
// the binary is missing — npm can skip install scripts — fetch it first.
const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");
module.exports = function run(bin) {
  const exe = path.join(__dirname, "vendor", bin);
  if (!fs.existsSync(exe)) {
    const fetch = spawnSync(process.execPath, [path.join(__dirname, "install.js")], { stdio: "inherit" });
    if (fetch.status !== 0) process.exit(fetch.status === null ? 1 : fetch.status);
  }
  const r = spawnSync(exe, process.argv.slice(2), { stdio: "inherit" });
  if (r.error) {
    console.error(`${bin}: ${r.error.message}`);
    process.exit(1);
  }
  process.exit(r.status === null ? 1 : r.status);
};
