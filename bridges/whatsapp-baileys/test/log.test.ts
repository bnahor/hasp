import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { EventLog, LogCorruptError } from "../src/log.ts";
import type { UnsequencedEvent } from "../src/normalize.ts";
import { ALICE_PN, OWNER_PN, tmpDir } from "./helpers.ts";

const ev = (id: string, chat = ALICE_PN): UnsequencedEvent => ({
  chat,
  id,
  fromMe: false,
  sender: chat,
  pushName: null,
  text: `t ${id}`,
  quotedId: null,
  timestamp: 1760780000,
  isGroup: false,
  attachments: [],
});

test("seq is monotonic and continues after restart", () => {
  const p = path.join(tmpDir(), "events.jsonl");
  const a = new EventLog(p);
  assert.equal(a.append(ev("1"))!.seq, 1);
  assert.equal(a.append(ev("2"))!.seq, 2);
  a.close();
  const b = new EventLog(p);
  assert.equal(b.lastSeq, 2);
  assert.equal(b.append(ev("3"))!.seq, 3);
  assert.deepEqual(b.read(0).events.map((e) => e.seq), [1, 2, 3]);
  b.close();
  // one event per line, persisted
  assert.equal(fs.readFileSync(p, "utf8").trim().split("\n").length, 3);
});

test("dedup on (chat, id), including across restart", () => {
  const p = path.join(tmpDir(), "events.jsonl");
  const a = new EventLog(p);
  assert.ok(a.append(ev("X")));
  assert.equal(a.append(ev("X")), null);
  assert.ok(a.append(ev("X", OWNER_PN)), "same id in another chat is a different message");
  a.close();
  const b = new EventLog(p);
  assert.equal(b.append(ev("X")), null);
  assert.equal(b.append(ev("X", OWNER_PN)), null);
  assert.equal(b.lastSeq, 2);
  b.close();
});

test("torn final line is moved aside and the file truncated to the valid prefix", () => {
  const p = path.join(tmpDir(), "events.jsonl");
  const a = new EventLog(p);
  a.append(ev("1"));
  a.append(ev("2"));
  a.close();
  const good = fs.statSync(p).size;
  fs.appendFileSync(p, '{"seq":3,"chat":"447700900002@s.whatsapp.net","id":"3","fr');
  const b = new EventLog(p);
  assert.equal(b.tornFile, `${p}.torn-${good}`);
  assert.equal(fs.readFileSync(b.tornFile!, "utf8"), '{"seq":3,"chat":"447700900002@s.whatsapp.net","id":"3","fr');
  assert.equal(fs.statSync(p).size, good);
  assert.equal(b.lastSeq, 2);
  // the torn message is not considered seen: a redelivery gets the next seq
  assert.equal(b.append(ev("3"))!.seq, 3);
  b.close();
  const c = new EventLog(p);
  assert.deepEqual(c.read(0).events.map((e) => e.id), ["1", "2", "3"]);
  c.close();
});

test("mid-file corruption is an error, not skipped", () => {
  const p = path.join(tmpDir(), "events.jsonl");
  const a = new EventLog(p);
  a.append(ev("1"));
  a.close();
  fs.appendFileSync(p, "garbage\n");
  fs.appendFileSync(p, JSON.stringify({ seq: 3, ...ev("3") }) + "\n");
  assert.throws(() => new EventLog(p), LogCorruptError);
  // the file is left untouched
  assert.equal(fs.readFileSync(p, "utf8").split("\n").length, 4);
});

test("non-increasing seq or a blank line is corruption too", () => {
  const p = path.join(tmpDir(), "events.jsonl");
  fs.writeFileSync(p, JSON.stringify({ seq: 2, ...ev("a") }) + "\n" + JSON.stringify({ seq: 2, ...ev("b") }) + "\n");
  assert.throws(() => new EventLog(p), /does not increase/);
  fs.writeFileSync(p, JSON.stringify({ seq: 1, ...ev("a") }) + "\n\n");
  assert.throws(() => new EventLog(p), /empty line/);
});

test("read: after filtering, limit and next", () => {
  const l = new EventLog(path.join(tmpDir(), "events.jsonl"));
  for (const id of ["1", "2", "3", "4"]) l.append(ev(id));
  assert.deepEqual(l.read(2), { events: l.read(0).events.slice(2), next: 4 });
  assert.deepEqual(l.read(4), { events: [], next: 4 });
  assert.deepEqual(l.read(9), { events: [], next: 9 });
  assert.equal(l.read(0, 2).next, 2);
  l.close();
});

test("waitFor resolves on append, on timeout, and on abort", async () => {
  const l = new EventLog(path.join(tmpDir(), "events.jsonl"));
  const p = l.waitFor(0, 5000);
  setTimeout(() => l.append(ev("1")), 20);
  assert.equal(await p, true);
  assert.equal(await l.waitFor(1, 20), false);
  const ac = new AbortController();
  const q = l.waitFor(1, 5000, ac.signal);
  ac.abort();
  assert.equal(await q, false);
  l.close();
});
