// Entry point. Env:
//   BRIDGE_PORT        loopback port (default 8790)
//   BRIDGE_TOKEN_FILE  file holding the bearer token, mode 0600 (required)
//   BRIDGE_DATA_DIR    event log, LID map, message cache, auth (default ./data)
//   BRIDGE_LOG_LEVEL   Baileys log level to stderr (default warn)
//
// Baileys is imported dynamically here and nowhere else.

import fs from "node:fs";
import path from "node:path";
import { EventLog } from "./log.ts";
import { createBridgeServer, listen, readTokenFile } from "./server.ts";
import type { Baileys } from "./socket.ts";
import { startSocket } from "./socket.ts";
import { LidStore, MessageCache, type BridgeState } from "./store.ts";
import type { WAMessageLike } from "./normalize.ts";

async function main(): Promise<void> {
  const port = Number(process.env.BRIDGE_PORT ?? 8790);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error(`bad BRIDGE_PORT ${process.env.BRIDGE_PORT}`);
  const tokenFile = process.env.BRIDGE_TOKEN_FILE;
  if (!tokenFile) throw new Error("BRIDGE_TOKEN_FILE is required (a file with the bearer token, chmod 600)");
  const token = readTokenFile(tokenFile);
  const dataDir = path.resolve(process.env.BRIDGE_DATA_DIR ?? "data");
  fs.mkdirSync(dataDir, { recursive: true, mode: 0o700 });

  const baileys = (await import("@whiskeysockets/baileys")) as Baileys;

  const log = new EventLog(path.join(dataDir, "events.jsonl"));
  if (log.tornFile) process.stderr.write(`bridge: recovered torn log tail into ${log.tornFile}\n`);
  const lids = new LidStore(path.join(dataDir, "lid-map.json"));
  const cache = new MessageCache<WAMessageLike>(path.join(dataDir, "messages.jsonl"), 2000, {
    replacer: baileys.BufferJSON.replacer,
    reviver: baileys.BufferJSON.reviver,
  });
  const state: BridgeState = { connected: false, me: { pn: null, lid: null } };

  const sock = await startSocket({
    baileys,
    authDir: path.join(dataDir, "auth"),
    state,
    lids,
    log,
    cache,
    logLevel: process.env.BRIDGE_LOG_LEVEL,
  });

  const server = createBridgeServer({ token, log, state, sendFn: sock.sendFn });
  const addr = await listen(server, port);
  process.stderr.write(`bridge: listening on http://${addr.address}:${addr.port} (last seq ${log.lastSeq})\n`);

  const shutdown = () => {
    sock.close();
    server.close();
    server.closeAllConnections();
    log.close();
    setTimeout(() => process.exit(0), 500).unref();
  };
  process.once("SIGINT", shutdown);
  process.once("SIGTERM", shutdown);
}

main().catch((e) => {
  process.stderr.write(`bridge: ${e instanceof Error ? e.message : String(e)}\n`);
  process.exit(1);
});
