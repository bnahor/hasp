import assert from "node:assert/strict";
import fs from "node:fs";
import type http from "node:http";
import path from "node:path";
import { after, test } from "node:test";
import { fileURLToPath } from "node:url";
import { EventLog } from "../src/log.ts";
import type { UnsequencedEvent } from "../src/normalize.ts";
import { HOST, NotConnectedError, createBridgeServer, listen, readTokenFile, type SendFn } from "../src/server.ts";
import { ingestUpsert } from "../src/socket.ts";
import { LidStore, MessageCache, type BridgeState } from "../src/store.ts";
import { ALICE_LID, ALICE_PN, GROUP, OWNER_LID, OWNER_PN, fakeJids, long, msg, tmpDir } from "./helpers.ts";

const TOKEN = "test-token-0123456789abcdef";
const servers: http.Server[] = [];
after(() => servers.forEach((s) => (s.close(), s.closeAllConnections())));

interface Harness {
  base: string;
  log: EventLog;
  state: BridgeState;
  calls: Array<[string, string, string | undefined]>;
  server: http.Server;
}

async function start(sendFn?: SendFn, sendTimeoutMs = 200): Promise<Harness> {
  const log = new EventLog(path.join(tmpDir(), "events.jsonl"));
  const state: BridgeState = { connected: true, me: { pn: OWNER_PN, lid: OWNER_LID } };
  const calls: Harness["calls"] = [];
  const fn: SendFn = sendFn ?? (async (jid, text, q) => (calls.push([jid, text, q]), { id: "3EB0SENT0001" }));
  const server = createBridgeServer({ token: TOKEN, log, state, sendFn: fn, sendTimeoutMs });
  servers.push(server);
  const addr = await listen(server, 0);
  return { base: `http://${addr.address}:${addr.port}`, log, state, calls, server };
}

const auth = { authorization: `Bearer ${TOKEN}` };
async function get(h: Harness, p: string, headers: Record<string, string> = auth) {
  const r = await fetch(h.base + p, { headers });
  return { status: r.status, body: (await r.json()) as any };
}
async function post(h: Harness, body: unknown) {
  const r = await fetch(h.base + "/send", {
    method: "POST",
    headers: { ...auth, "content-type": "application/json" },
    body: typeof body === "string" ? body : JSON.stringify(body),
  });
  return { status: r.status, body: (await r.json()) as any };
}
const ev = (id: string): UnsequencedEvent => ({
  chat: ALICE_PN, id, fromMe: false, sender: ALICE_PN, pushName: null, text: id, quotedId: null, timestamp: 1, isGroup: false, attachments: [],
});

test("binds 127.0.0.1 only", async () => {
  const h = await start();
  assert.equal(HOST, "127.0.0.1");
  assert.equal((h.server.address() as { address: string }).address, "127.0.0.1");
});

test("401 without a token or with a wrong one, on every route", async () => {
  const h = await start();
  for (const p of ["/health", "/events?after=0", "/nope"]) {
    assert.deepEqual(await get(h, p, {}), { status: 401, body: { error: "unauthorized" } });
    assert.equal((await get(h, p, { authorization: "Bearer wrong" })).status, 401);
    assert.equal((await get(h, p, { authorization: TOKEN })).status, 401, "missing Bearer prefix");
  }
  const r = await fetch(h.base + "/send", { method: "POST", body: JSON.stringify({ jid: ALICE_PN, text: "x" }) });
  assert.equal(r.status, 401);
  assert.equal(h.calls.length, 0);
});

test("health", async () => {
  const h = await start();
  h.log.append(ev("a"));
  assert.deepEqual((await get(h, "/health")).body, { connected: true, me: { pn: OWNER_PN, lid: OWNER_LID }, lastEventSeq: 1 });
});

test("events: after filtering and next", async () => {
  const h = await start();
  for (const id of ["a", "b", "c"]) h.log.append(ev(id));
  const r = await get(h, "/events?after=1&wait=0");
  assert.deepEqual(r.body.events.map((e: any) => e.seq), [2, 3]);
  assert.equal(r.body.next, 3);
  const empty = await get(h, "/events?after=3");
  assert.deepEqual(empty.body, { events: [], next: 3 });
  assert.equal((await get(h, "/events?after=-1")).status, 400);
  assert.equal((await get(h, "/events?after=x")).status, 400);
});

test("events: long poll returns early when an event arrives", async () => {
  const h = await start();
  const t0 = Date.now();
  const p = get(h, "/events?after=0&wait=20");
  setTimeout(() => h.log.append(ev("late")), 150);
  const r = await p;
  const dt = Date.now() - t0;
  assert.equal(r.body.events.length, 1);
  assert.equal(r.body.next, 1);
  assert.ok(dt >= 100 && dt < 3000, `took ${dt} ms`);
});

test("events: long poll times out with no events", async () => {
  const h = await start();
  const t0 = Date.now();
  const r = await get(h, "/events?after=0&wait=0.3");
  assert.deepEqual(r.body, { events: [], next: 0 });
  assert.ok(Date.now() - t0 >= 250);
});

test("send 200 passes jid, text and quotedId", async () => {
  const h = await start();
  const r = await post(h, { jid: OWNER_PN, text: "hello", quotedId: "Q1" });
  assert.deepEqual(r, { status: 200, body: { id: "3EB0SENT0001" } });
  assert.deepEqual(h.calls, [[OWNER_PN, "hello", "Q1"]]);
  await post(h, { jid: GROUP, text: "g" });
  assert.deepEqual(h.calls[1], [GROUP, "g", undefined]);
});

test("send 503 when not connected (nothing handed to the socket) or socket reports not connected", async () => {
  const h = await start();
  h.state.connected = false;
  const r = await post(h, { jid: OWNER_PN, text: "x" });
  assert.equal(r.status, 503);
  assert.equal(typeof r.body.error, "string");
  assert.equal(h.calls.length, 0);
  const h2 = await start(async () => {
    throw new NotConnectedError("whatsapp connection closed");
  });
  assert.deepEqual(await post(h2, { jid: OWNER_PN, text: "x" }), { status: 503, body: { error: "whatsapp connection closed" } });
});

test("send 504 when handed to the socket but no result in time", async () => {
  const h = await start(() => new Promise(() => {}), 100);
  const r = await post(h, { jid: OWNER_PN, text: "x" });
  assert.equal(r.status, 504);
  assert.match(r.body.error, /within 100 ms/);
});

test("send 400 on bad input", async () => {
  const h = await start();
  for (const body of ["not json", "[]", { text: "x" }, { jid: OWNER_PN }, { jid: OWNER_PN, text: "" }, { jid: "+44 7700", text: "x" }, { jid: "status@broadcast", text: "x" }, { jid: OWNER_PN, text: "x", quotedId: 5 }]) {
    const r = await post(h, body);
    assert.equal(r.status, 400, JSON.stringify(body));
    assert.equal(typeof r.body.error, "string");
  }
  assert.equal(h.calls.length, 0);
});

test("token file must be 0600", () => {
  const d = tmpDir();
  const f = path.join(d, "token");
  fs.writeFileSync(f, TOKEN + "\n", { mode: 0o644 });
  fs.chmodSync(f, 0o644);
  assert.throws(() => readTokenFile(f), /chmod 600/);
  fs.chmodSync(f, 0o600);
  assert.equal(readTokenFile(f), TOKEN);
});

// ------------------------------------------------------------------ contract fixture
//
// test/fixtures/events.json is shared with the Rust adapter tests. It is
// produced by running sample WAMessages through the real pipeline
// (normalize -> log -> HTTP). Regenerate with UPDATE_FIXTURES=1 npm test.

const FIXTURE = path.join(path.dirname(fileURLToPath(import.meta.url)), "fixtures", "events.json");

const SAMPLE_UPSERT = {
  type: "notify",
  messages: [
    // owner writes in "Message yourself" (LID addressing, mapped via remoteJidAlt)
    msg({ remoteJid: OWNER_LID, remoteJidAlt: OWNER_PN, id: "3EB0SELF0001", fromMe: true }, { conversation: "/status" }, { pushName: "Owner", messageTimestamp: 1760780000 }),
    // owner sends a voice note to themselves
    msg({ remoteJid: OWNER_PN, id: "3EB0SELF0002", fromMe: true }, { audioMessage: { ptt: true, mimetype: "audio/ogg; codecs=opus", fileLength: long(5120) } }, { pushName: "Owner", messageTimestamp: 1760780005 }),
    // a contact replies to something, quoting it (LID DM)
    msg({ remoteJid: ALICE_LID, remoteJidAlt: ALICE_PN, id: "ABCDEF0001", fromMe: false }, { extendedTextMessage: { text: "sounds good", contextInfo: { stanzaId: "3EB0SENT0001" } } }, { pushName: "Alice", messageTimestamp: 1760780010 }),
    // group photo with caption
    msg({ remoteJid: GROUP, id: "ABCDEF0002", participant: "447700900002:4@s.whatsapp.net", fromMe: false }, { imageMessage: { caption: "look at this", mimetype: "image/jpeg", fileLength: 48213 } }, { pushName: "Alice", messageTimestamp: 1760780020 }),
    // a document in a DM
    msg({ remoteJid: ALICE_PN, id: "ABCDEF0003", fromMe: false }, { documentMessage: { fileName: "invoice.pdf", mimetype: "application/pdf", fileLength: 90210 } }, { pushName: "Alice", messageTimestamp: 1760780030 }),
    // owner typing to someone else from their phone
    msg({ remoteJid: ALICE_PN, id: "3EB0OUT0001", fromMe: true }, { conversation: "on my way" }, { pushName: "Owner", messageTimestamp: 1760780040 }),
  ],
};

test("contract fixture matches the real pipeline", async () => {
  const sendFn: SendFn = async (_jid, text) => {
    if (text === "timeout") return new Promise(() => {});
    if (text === "offline") throw new NotConnectedError("whatsapp socket not connected");
    return { id: "3EB0SENT0002" };
  };
  const h = await start(sendFn, 100);
  const dir = tmpDir();
  ingestUpsert(
    { jids: fakeJids, state: h.state, lids: new LidStore(path.join(dir, "lid.json")), log: h.log, cache: new MessageCache(path.join(dir, "m.jsonl")) },
    SAMPLE_UPSERT,
  );
  // a redelivery of the same message must not get a new seq
  ingestUpsert({ jids: fakeJids, state: h.state, lids: new LidStore(path.join(dir, "lid.json")), log: h.log, cache: new MessageCache(path.join(dir, "m.jsonl")) }, { type: "notify", messages: [SAMPLE_UPSERT.messages[0]!] });

  const ex = async (method: "GET" | "POST", p: string, body?: unknown, headers: Record<string, string> = auth) => {
    const r = await fetch(h.base + p, { method, headers: { ...headers, "content-type": "application/json" }, body: body === undefined ? undefined : JSON.stringify(body) });
    return { request: { method, path: p, ...(body === undefined ? {} : { body }) }, status: r.status, body: await r.json() };
  };
  const actual = {
    _comment: "Generated by test/server.test.ts from sample WAMessages through normalize -> log -> HTTP. Regenerate with UPDATE_FIXTURES=1 npm test.",
    self_jids: [OWNER_PN, OWNER_LID],
    health: await ex("GET", "/health"),
    events_responses: [await ex("GET", "/events?after=0&wait=25"), await ex("GET", "/events?after=3&wait=0"), await ex("GET", "/events?after=6&wait=0")],
    send_replies: [
      await ex("POST", "/send", { jid: OWNER_PN, text: "Here is your status.", quotedId: "3EB0SELF0001" }),
      await ex("POST", "/send", { jid: OWNER_PN, text: "offline" }),
      await ex("POST", "/send", { jid: OWNER_PN, text: "timeout" }),
      await ex("POST", "/send", { jid: OWNER_PN, text: "" }),
      await ex("POST", "/send", { jid: OWNER_PN, text: "x" }, { authorization: "Bearer wrong" }),
    ],
  };
  if (process.env.UPDATE_FIXTURES) fs.writeFileSync(FIXTURE, JSON.stringify(actual, null, 2) + "\n");
  const expected = JSON.parse(fs.readFileSync(FIXTURE, "utf8"));
  assert.deepEqual(actual, expected);

  // shape check: exactly the contract's field names
  const keys = ["seq", "chat", "id", "fromMe", "sender", "pushName", "text", "quotedId", "timestamp", "isGroup", "attachments"];
  for (const e of expected.events_responses[0].body.events) {
    assert.deepEqual(Object.keys(e).sort(), [...keys].sort());
    for (const a of e.attachments) assert.deepEqual(Object.keys(a).sort(), ["id", "kind", "mime", "name", "size"]);
  }
  const chats = expected.events_responses[0].body.events.map((e: any) => e.chat);
  assert.equal(chats[0], OWNER_PN, "self chat comes out as one of the me JIDs");
  assert.equal(expected.events_responses[0].body.next, 6);
});
