// Loopback HTTP API consumed by crates/hasp-gateway/src/adapters/whatsapp.rs.
//
//   GET  /events?after=<seq>&wait=<s>  long poll (wait capped at 30 s)
//   POST /send {"jid","text","quotedId"?}
//   GET  /health
//
// Every route needs `Authorization: Bearer <token>`. Binds 127.0.0.1 only.

import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import type { AddressInfo } from "node:net";
import type { EventLog } from "./log.ts";
import type { BridgeState } from "./store.ts";

export const HOST = "127.0.0.1";
export const MAX_WAIT_S = 30;
export const MAX_EVENTS_PER_POLL = 500;
const MAX_BODY = 256 * 1024;
const MAX_TEXT = 65_536;
const JID_RE = /^[0-9A-Za-z._:-]+@(s\.whatsapp\.net|lid|g\.us)$/;

/** Thrown by sendFn when the socket is not connected and nothing was sent (-> 503). */
export class NotConnectedError extends Error {}
/** Thrown by sendFn for input the socket rejects before sending (-> 400). */
export class BadRequestError extends Error {}

export type SendFn = (jid: string, text: string, quotedId?: string) => Promise<{ id: string }>;

export interface ServerOpts {
  token: string;
  log: EventLog;
  state: BridgeState;
  sendFn: SendFn;
  /** How long to wait for the socket to hand back a sent message (-> 504). */
  sendTimeoutMs?: number;
}

/** Read the bearer token from a file that must be mode 0600 (no group/other bits). */
export function readTokenFile(path: string): string {
  const st = fs.statSync(path);
  if (!st.isFile()) throw new Error(`${path}: not a regular file`);
  if ((st.mode & 0o077) !== 0) {
    throw new Error(`${path}: permissions ${(st.mode & 0o777).toString(8)}; run chmod 600 ${path}`);
  }
  const t = fs.readFileSync(path, "utf8").trim();
  if (t.length < 16) throw new Error(`${path}: token must be at least 16 characters`);
  return t;
}

function tokenOk(header: string | undefined, token: string): boolean {
  if (!header?.startsWith("Bearer ")) return false;
  const a = crypto.createHash("sha256").update(header.slice(7)).digest();
  const b = crypto.createHash("sha256").update(token).digest();
  return crypto.timingSafeEqual(a, b);
}

function reply(res: http.ServerResponse, status: number, body: unknown): void {
  if (res.headersSent || res.destroyed) return;
  const data = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(data) });
  res.end(data);
}

function readBody(req: http.IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let n = 0;
    req.on("data", (c: Buffer) => {
      n += c.length;
      if (n > MAX_BODY) {
        reject(new BadRequestError("body too large"));
        req.destroy();
      } else chunks.push(c);
    });
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    req.on("error", reject);
  });
}

function parseNonNegInt(v: string | null, def: number): number | null {
  if (v === null || v === "") return def;
  if (!/^\d+$/.test(v)) return null;
  const n = Number(v);
  return Number.isSafeInteger(n) ? n : null;
}

class TimeoutError extends Error {}

function withTimeout<T>(p: Promise<T>, ms: number): Promise<T> {
  let t: NodeJS.Timeout;
  return Promise.race([
    p.finally(() => clearTimeout(t)),
    new Promise<never>((_, rej) => {
      t = setTimeout(() => rej(new TimeoutError(`no result from WhatsApp within ${ms} ms`)), ms);
    }),
  ]);
}

export function createBridgeServer(opts: ServerOpts): http.Server {
  const sendTimeoutMs = opts.sendTimeoutMs ?? 30_000;

  async function events(req: http.IncomingMessage, res: http.ServerResponse, url: URL) {
    const after = parseNonNegInt(url.searchParams.get("after"), 0);
    const waitRaw = url.searchParams.get("wait");
    const wait = waitRaw === null || waitRaw === "" ? 0 : Number(waitRaw);
    if (after === null || !Number.isFinite(wait) || wait < 0) {
      return reply(res, 400, { error: "after must be a non-negative integer and wait a non-negative number" });
    }
    const ac = new AbortController();
    const onClose = () => ac.abort();
    res.on("close", onClose);
    try {
      await opts.log.waitFor(after, Math.min(wait, MAX_WAIT_S) * 1000, ac.signal);
    } finally {
      res.off("close", onClose);
    }
    reply(res, 200, opts.log.read(after, MAX_EVENTS_PER_POLL));
  }

  async function send(req: http.IncomingMessage, res: http.ServerResponse) {
    let body: unknown;
    try {
      body = JSON.parse(await readBody(req));
    } catch {
      return reply(res, 400, { error: "body must be JSON" });
    }
    const b = body as Record<string, unknown> | null;
    if (!b || typeof b !== "object" || Array.isArray(b)) return reply(res, 400, { error: "body must be an object" });
    const { jid, text, quotedId } = b;
    if (typeof jid !== "string" || !JID_RE.test(jid)) return reply(res, 400, { error: "jid must be a user, lid or group JID" });
    if (typeof text !== "string" || text.length === 0 || text.length > MAX_TEXT) {
      return reply(res, 400, { error: `text must be a non-empty string of at most ${MAX_TEXT} chars` });
    }
    if (quotedId !== undefined && quotedId !== null && (typeof quotedId !== "string" || quotedId === "")) {
      return reply(res, 400, { error: "quotedId must be a non-empty string" });
    }
    if (!opts.state.connected) return reply(res, 503, { error: "whatsapp socket not connected" });
    try {
      const r = await withTimeout(opts.sendFn(jid, text, (quotedId as string | null) ?? undefined), sendTimeoutMs);
      reply(res, 200, { id: r.id });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      if (e instanceof NotConnectedError) reply(res, 503, { error: msg || "whatsapp socket not connected" });
      else if (e instanceof TimeoutError) reply(res, 504, { error: msg });
      else if (e instanceof BadRequestError) reply(res, 400, { error: msg });
      else reply(res, 500, { error: msg });
    }
  }

  const server = http.createServer((req, res) => {
    if (!tokenOk(req.headers.authorization, opts.token)) return reply(res, 401, { error: "unauthorized" });
    const url = new URL(req.url ?? "/", "http://localhost");
    const route = `${req.method} ${url.pathname}`;
    const fail = (e: unknown) => reply(res, 500, { error: e instanceof Error ? e.message : String(e) });
    switch (route) {
      case "GET /events":
        return void events(req, res, url).catch(fail);
      case "POST /send":
        return void send(req, res).catch(fail);
      case "GET /health":
        return reply(res, 200, {
          connected: opts.state.connected,
          me: { pn: opts.state.me.pn, lid: opts.state.me.lid },
          lastEventSeq: opts.log.lastSeq,
        });
      default:
        return reply(res, 404, { error: "not found" });
    }
  });
  // long polls run up to 30 s
  server.requestTimeout = (MAX_WAIT_S + 30) * 1000;
  return server;
}

/** Start listening on 127.0.0.1 (never any other interface). */
export function listen(server: http.Server, port: number): Promise<AddressInfo> {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, HOST, () => {
      server.off("error", reject);
      resolve(server.address() as AddressInfo);
    });
  });
}
