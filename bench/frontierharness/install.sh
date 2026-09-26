#!/bin/sh
# FrontierHarness --install-script: runs in /work/harness on the build runtime,
# before egress is restricted. Builds a static x86_64 binary in Docker (the
# runtime already has Docker for Harbor/Pier), so it runs in any task image.
set -eu
cd "${HARNESS_DIR:-/work/harness}"
out=target/fh
docker run --rm --platform linux/amd64 -v "$PWD":/src -w /src -e CARGO_TARGET_DIR=/src/$out \
  rust:1-alpine sh -c 'apk add --no-cache musl-dev >/dev/null && cargo build --release -p harness-cli'
test -x $out/release/harness
git rev-parse --short HEAD > $out/release/harness.version
file $out/release/harness 2>/dev/null || true
