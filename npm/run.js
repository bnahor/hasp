"use strict";
// Run a vendored binary with this process's arguments and exit status.
const path = require("path");
const { spawnSync } = require("child_process");
module.exports = function run(bin) {
  const exe = path.join(__dirname, "vendor", bin);
  const r = spawnSync(exe, process.argv.slice(2), { stdio: "inherit" });
  if (r.error) {
    console.error(`${bin}: ${r.error.code === "ENOENT" ? "not installed — reinstall hasp-cli" : r.error.message}`);
    process.exit(1);
  }
  process.exit(r.status === null ? 1 : r.status);
};
