// Download this version's prebuilt binaries from the GitHub release, verify
// the checksum, and unpack them into ./vendor. Node built-ins only.
"use strict";
const crypto = require("crypto");
const fs = require("fs");
const https = require("https");
const path = require("path");
const { execFileSync } = require("child_process");

const REPO = process.env.HASP_REPO || "bnahor/hasp";
const version = require("./package.json").version;
const targets = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-x64": "x86_64-unknown-linux-gnu",
};
const target = targets[`${process.platform}-${process.arch}`];
if (!target) {
  console.error(`hasp: no prebuilt binary for ${process.platform}-${process.arch}; build from source: https://github.com/${REPO}#install`);
  process.exit(1);
}
const name = `hasp-${target}`;
const base = `https://github.com/${REPO}/releases/download/v${version}/${name}.tar.gz`;

function get(url, redirects = 5) {
  return new Promise((resolve, reject) => {
    https.get(url, { headers: { "user-agent": "hasp-npm-installer" } }, res => {
      if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location && redirects > 0) {
        res.resume();
        return resolve(get(new URL(res.headers.location, url).toString(), redirects - 1));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`GET ${url} → HTTP ${res.statusCode}`));
      }
      const chunks = [];
      res.on("data", c => chunks.push(c));
      res.on("end", () => resolve(Buffer.concat(chunks)));
    }).on("error", reject);
  });
}

(async () => {
  const [tarball, sums] = await Promise.all([get(base), get(`${base}.sha256.txt`)]);
  const want = sums.toString().trim().split(/\s+/)[0];
  const got = crypto.createHash("sha256").update(tarball).digest("hex");
  if (want !== got) throw new Error(`checksum mismatch for ${name}.tar.gz`);
  const vendor = path.join(__dirname, "vendor");
  fs.rmSync(vendor, { recursive: true, force: true });
  fs.mkdirSync(vendor, { recursive: true });
  const archive = path.join(vendor, `${name}.tar.gz`);
  fs.writeFileSync(archive, tarball);
  execFileSync("tar", ["-xzf", archive, "-C", vendor, "--strip-components=1"]);
  fs.rmSync(archive);
  for (const bin of ["hasp", "hasp-desktop"]) {
    const p = path.join(vendor, bin);
    if (fs.existsSync(p)) fs.chmodSync(p, 0o755);
  }
  console.log(`hasp ${version} installed (${target})`);
})().catch(err => {
  console.error(`hasp: install failed: ${err.message}`);
  process.exit(1);
});
