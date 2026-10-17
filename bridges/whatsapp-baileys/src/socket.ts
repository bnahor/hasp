// Baileys wiring. The Baileys module itself is passed in (index.ts imports it
// dynamically); this file only has type imports, so loading it never pulls in
// Baileys and tests can exercise the pure parts (reconnectPlan, ingest).
//
// The QR code is printed to stderr for the owner to scan by hand. Nothing in
// this bridge scans it, requests a pairing code, or links an account.

import type * as BaileysNS from "@whiskeysockets/baileys";
import type { EventLog } from "./log.ts";
import {
  extractMappings,
  meFromContact,
  normalizeUpsert,
  type JidShim,
  type LidPn,
  type UpsertLike,
  type WAMessageLike,
} from "./normalize.ts";
import { NotConnectedError, type SendFn } from "./server.ts";
import { makeCtx, type BridgeState, type LidStore, type MessageCache } from "./store.ts";

export type Baileys = typeof BaileysNS;
type WAMessage = BaileysNS.WAMessage;
type GroupMetadata = BaileysNS.GroupMetadata;
type ILogger = NonNullable<Parameters<Baileys["makeCacheableSignalKeyStore"]>[1]>;

// DisconnectReason codes (see platform notes section 3).
export const LOGGED_OUT = 401;
export const FORBIDDEN = 403;
export const CONNECTION_CLOSED = 428;
export const CONNECTION_REPLACED = 440;
export const RESTART_REQUIRED = 515;

export type ReconnectPlan = { action: "stop"; reason: string } | { action: "reconnect"; delayMs: number };

/**
 * What to do after `connection: 'close'`.
 * - 401 loggedOut: stop; the owner must delete the auth dir and pair again.
 * - 440 connectionReplaced: stop; another session took over, don't fight it.
 * - 403 forbidden: stop (likely a ban or block; retrying makes it worse).
 * - 515 restartRequired (normal right after pairing): reconnect immediately.
 * - anything else: reconnect with exponential backoff (1 s .. 60 s).
 */
export function reconnectPlan(code: number | undefined, attempt: number): ReconnectPlan {
  if (code === LOGGED_OUT) return { action: "stop", reason: "logged out: delete the auth dir and pair again by hand" };
  if (code === CONNECTION_REPLACED) return { action: "stop", reason: "connection replaced by another session; restart the bridge to take over" };
  if (code === FORBIDDEN) return { action: "stop", reason: "forbidden (403): the account may be restricted" };
  if (code === RESTART_REQUIRED) return { action: "reconnect", delayMs: 0 };
  return { action: "reconnect", delayMs: Math.min(60_000, 1000 * 2 ** Math.min(attempt, 6)) };
}

export function statusCodeOf(err: unknown): number | undefined {
  const c = (err as { output?: { statusCode?: unknown } } | undefined)?.output?.statusCode;
  return typeof c === "number" ? c : undefined;
}

export interface Ingest {
  jids: JidShim;
  state: BridgeState;
  lids: LidStore;
  log: EventLog;
  cache: MessageCache<WAMessageLike>;
}

/** Handle one messages.upsert: learn LID<->PN pairs, then (notify only) cache and log. */
export function ingestUpsert(d: Ingest, up: UpsertLike): number {
  const pairs: LidPn[] = [];
  for (const m of up.messages) pairs.push(...extractMappings(m, d.jids));
  if (pairs.length) d.lids.add(pairs);
  if (up.type !== "notify") return 0;
  for (const m of up.messages) d.cache.put(m);
  let n = 0;
  for (const e of normalizeUpsert(up, makeCtx(d.jids, d.state, d.lids))) if (d.log.append(e)) n++;
  return n;
}

/** Stderr-only logger matching Baileys' ILogger; quiet by default. */
function makeLogger(level: string): ILogger {
  const order = ["trace", "debug", "info", "warn", "error"];
  const min = Math.max(0, order.indexOf(level));
  const at = (lvl: string) => (obj: unknown, msg?: string) => {
    if (order.indexOf(lvl) < min) return;
    let o: string;
    try {
      o = typeof obj === "string" ? obj : JSON.stringify(obj);
    } catch {
      o = String(obj);
    }
    process.stderr.write(`[baileys ${lvl}] ${msg ?? ""} ${o}\n`);
  };
  const logger: ILogger = {
    level,
    child: () => logger,
    trace: at("trace"),
    debug: at("debug"),
    info: at("info"),
    warn: at("warn"),
    error: at("error"),
  };
  return logger;
}

async function printQr(qr: string): Promise<void> {
  process.stderr.write("\nScan this QR with WhatsApp > Linked devices > Link a device (owner only):\n");
  try {
    const mod = (await import("qrcode-terminal" as string)) as {
      default: { generate(t: string, o: { small: boolean }, cb: (s: string) => void): void };
    };
    mod.default.generate(qr, { small: true }, (s) => process.stderr.write(s + "\n"));
  } catch {
    process.stderr.write(`(qrcode-terminal missing; render this string as a QR yourself)\n${qr}\n`);
  }
}

export interface SocketOpts extends Omit<Ingest, "jids"> {
  baileys: Baileys;
  authDir: string;
  logLevel?: string;
}

export interface BridgeSocket {
  sendFn: SendFn;
  close(): void;
}

export async function startSocket(o: SocketOpts): Promise<BridgeSocket> {
  const b = o.baileys;
  const jids: JidShim = b;
  const d: Ingest = { jids, state: o.state, lids: o.lids, log: o.log, cache: o.cache };
  const logger = makeLogger(o.logLevel ?? "warn");
  // Upstream warns useMultiFileAuthState is not meant for production; for a
  // single personal account on a backed-up local disk it is tolerable.
  const { state: auth, saveCreds } = await b.useMultiFileAuthState(o.authDir);

  const groups = new Map<string, { at: number; meta: GroupMetadata }>();
  const GROUP_TTL_MS = 5 * 60_000;

  let sock: ReturnType<Baileys["makeWASocket"]> | null = null;
  let stopped = false;
  let attempt = 0;
  let timer: NodeJS.Timeout | undefined;

  const updateMe = (c: { id?: string | null; lid?: string | null; phoneNumber?: string | null } | undefined) => {
    const me = meFromContact(c, jids);
    if (me.pn || me.lid) o.state.me = { pn: me.pn ?? o.state.me.pn, lid: me.lid ?? o.state.me.lid };
    if (o.state.me.pn && o.state.me.lid) o.lids.add([{ lid: o.state.me.lid, pn: o.state.me.pn }]);
  };
  updateMe(auth.creds.me);

  const connect = () => {
    if (stopped) return;
    const s = b.makeWASocket({
      auth: { creds: auth.creds, keys: b.makeCacheableSignalKeyStore(auth.keys, logger) },
      logger,
      markOnlineOnConnect: false, // keep phone push notifications working
      syncFullHistory: false,
      getMessage: async (key) => (key.id ? (o.cache.get(key.id) as WAMessage | undefined)?.message ?? undefined : undefined),
      cachedGroupMetadata: async (jid) => {
        const hit = groups.get(jid);
        if (hit && Date.now() - hit.at < GROUP_TTL_MS) return hit.meta;
        try {
          const meta = await s.groupMetadata(jid);
          groups.set(jid, { at: Date.now(), meta });
          return meta;
        } catch {
          return undefined;
        }
      },
    });
    sock = s;

    s.ev.on("creds.update", async () => {
      try {
        await saveCreds();
      } catch (e) {
        process.stderr.write(`bridge: saving creds failed: ${String(e)}\n`);
      }
      updateMe(auth.creds.me);
    });

    s.ev.on("connection.update", (u) => {
      if (sock !== s) return;
      if (u.qr) void printQr(u.qr);
      if (u.connection === "open") {
        attempt = 0;
        updateMe(s.user ?? auth.creds.me);
        o.state.connected = true;
        process.stderr.write(`bridge: connected as ${o.state.me.pn ?? "?"} / ${o.state.me.lid ?? "?"}\n`);
      } else if (u.connection === "close") {
        o.state.connected = false;
        if (stopped) return;
        const code = statusCodeOf(u.lastDisconnect?.error);
        const plan = reconnectPlan(code, attempt++);
        if (plan.action === "stop") {
          process.stderr.write(`bridge: not reconnecting (${code}): ${plan.reason}\n`);
          return;
        }
        process.stderr.write(`bridge: connection closed (${code ?? "?"}); reconnecting in ${plan.delayMs} ms\n`);
        timer = setTimeout(connect, plan.delayMs);
      }
    });

    s.ev.on("messages.upsert", (up) => {
      try {
        ingestUpsert(d, up as UpsertLike);
      } catch (e) {
        // A failed log write must not be swallowed silently: the event would be lost.
        process.stderr.write(`bridge: FAILED to persist upsert: ${String(e)}\n`);
        process.exitCode = 1;
        stopped = true;
        s.end(e as Error);
      }
    });

    s.ev.on("lid-mapping.update", (m) => {
      const lid = jids.jidNormalizedUser(m.lid);
      const pn = jids.jidNormalizedUser(m.pn);
      if (jids.isLidUser(lid) && jids.isPnUser(pn)) o.lids.add([{ lid, pn }]);
    });

    const invalidate = (id: string | undefined) => id && groups.delete(id);
    s.ev.on("groups.update", (list) => list.forEach((g) => invalidate(g.id ?? undefined)));
    s.ev.on("group-participants.update", (ev) => invalidate(ev.id));
  };

  connect();

  const sendFn: SendFn = async (jid, text, quotedId) => {
    const s = sock;
    if (!s || !o.state.connected) throw new NotConnectedError("whatsapp socket not connected");
    const quoted = quotedId ? (o.cache.get(quotedId) as WAMessage | undefined) : undefined;
    if (quotedId && !quoted) process.stderr.write(`bridge: quotedId ${quotedId} not cached; sending unquoted\n`);
    let sent: WAMessage | undefined;
    try {
      sent = await s.sendMessage(jid, { text }, quoted ? { quoted } : undefined);
    } catch (e) {
      // "Connection Closed" is raised before the node is written: nothing went out.
      if (statusCodeOf(e) === CONNECTION_CLOSED) throw new NotConnectedError("whatsapp connection closed");
      throw e;
    }
    const id = sent?.key?.id;
    if (!sent || !id) throw new Error("socket returned no message key");
    o.cache.put(sent);
    return { id };
  };

  return {
    sendFn,
    close() {
      stopped = true;
      clearTimeout(timer);
      o.state.connected = false;
      sock?.end(undefined);
    },
  };
}
