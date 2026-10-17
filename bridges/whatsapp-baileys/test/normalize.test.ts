import assert from "node:assert/strict";
import path from "node:path";
import { test } from "node:test";
import { EventLog } from "../src/log.ts";
import { extractMappings, meFromContact, normalizeMessage, normalizeUpsert, type WAMessageLike } from "../src/normalize.ts";
import { ingestUpsert } from "../src/socket.ts";
import { LidStore, MessageCache, type BridgeState } from "../src/store.ts";
import {
  ALICE_LID,
  ALICE_PN,
  BOB_LID,
  GROUP,
  OWNER_LID,
  OWNER_PN,
  ctxWith,
  fakeJids,
  long,
  msg,
  ownerCtx,
  tmpDir,
} from "./helpers.ts";

test("plain conversation text in a DM", () => {
  const e = normalizeMessage(msg({ remoteJid: ALICE_PN, id: "A1", fromMe: false }, { conversation: "hi" }, { pushName: "Alice" }), ownerCtx());
  assert.deepEqual(e, {
    chat: ALICE_PN,
    id: "A1",
    fromMe: false,
    sender: ALICE_PN,
    pushName: "Alice",
    text: "hi",
    quotedId: null,
    timestamp: 1760780000,
    isGroup: false,
    attachments: [],
  });
});

test("extendedTextMessage text and quoted stanzaId", () => {
  const e = normalizeMessage(
    msg({ remoteJid: ALICE_PN, id: "A2" }, { extendedTextMessage: { text: "see this", contextInfo: { stanzaId: "Q1" } } }),
    ownerCtx(),
  )!;
  assert.equal(e.text, "see this");
  assert.equal(e.quotedId, "Q1");
});

test("image caption becomes text plus a photo attachment", () => {
  const e = normalizeMessage(
    msg({ remoteJid: ALICE_PN, id: "IMG1" }, { imageMessage: { caption: "look", mimetype: "image/jpeg", fileLength: long(12345) } }, { messageTimestamp: long(1760780001) }),
    ownerCtx(),
  )!;
  assert.equal(e.text, "look");
  assert.equal(e.timestamp, 1760780001);
  assert.deepEqual(e.attachments, [{ id: "IMG1", kind: "photo", mime: "image/jpeg", name: null, size: 12345 }]);
});

test("video caption", () => {
  const e = normalizeMessage(msg({ remoteJid: ALICE_PN, id: "V1" }, { videoMessage: { caption: "clip", mimetype: "video/mp4", fileLength: 9 } }), ownerCtx())!;
  assert.equal(e.text, "clip");
  assert.equal(e.attachments[0]!.kind, "video");
});

test("ptt audio is voice, other audio is audio", () => {
  const voice = normalizeMessage(msg({ remoteJid: ALICE_PN, id: "P1" }, { audioMessage: { ptt: true, mimetype: "audio/ogg; codecs=opus", fileLength: 4000 } }), ownerCtx())!;
  const audio = normalizeMessage(msg({ remoteJid: ALICE_PN, id: "P2" }, { audioMessage: { ptt: false, mimetype: "audio/mpeg", fileLength: 9000 } }), ownerCtx())!;
  assert.equal(voice.attachments[0]!.kind, "voice");
  assert.equal(voice.text, "");
  assert.equal(audio.attachments[0]!.kind, "audio");
  assert.equal(audio.attachments[0]!.mime, "audio/mpeg");
});

test("document with name, mime and caption (also inside documentWithCaptionMessage)", () => {
  const doc = { fileName: "report.pdf", mimetype: "application/pdf", fileLength: 2048, caption: "the report" };
  const plain = normalizeMessage(msg({ remoteJid: ALICE_PN, id: "D1" }, { documentMessage: doc }), ownerCtx())!;
  assert.equal(plain.text, "the report");
  assert.deepEqual(plain.attachments, [{ id: "D1", kind: "document", mime: "application/pdf", name: "report.pdf", size: 2048 }]);
  const wrapped = normalizeMessage(msg({ remoteJid: ALICE_PN, id: "D2" }, { documentWithCaptionMessage: { message: { documentMessage: doc } } }), ownerCtx())!;
  assert.equal(wrapped.attachments[0]!.name, "report.pdf");
});

test("group: sender is the normalized participant", () => {
  const e = normalizeMessage(
    msg({ remoteJid: GROUP, id: "G1", participant: "333333333333333:7@lid" }, { conversation: "group hi" }, { pushName: "Bob" }),
    ownerCtx(),
  )!;
  assert.equal(e.chat, GROUP);
  assert.equal(e.isGroup, true);
  assert.equal(e.sender, BOB_LID);
});

test("fromMe in the self chat: chat and sender are the owner's stable JID", () => {
  const e = normalizeMessage(msg({ remoteJid: "447700900001:3@s.whatsapp.net", id: "S1", fromMe: true }, { conversation: "/status" }), ownerCtx())!;
  assert.equal(e.chat, OWNER_PN);
  assert.equal(e.sender, OWNER_PN);
  assert.equal(e.fromMe, true);
});

test("fromMe in another chat: chat is the other party, sender is the owner", () => {
  const e = normalizeMessage(msg({ remoteJid: ALICE_PN, id: "S2", fromMe: true }, { conversation: "see you" }), ownerCtx())!;
  assert.equal(e.chat, ALICE_PN);
  assert.equal(e.sender, OWNER_PN);
  const g = normalizeMessage(msg({ remoteJid: GROUP, id: "S3", fromMe: true, participant: OWNER_LID }, { conversation: "x" }), ownerCtx())!;
  assert.equal(g.chat, GROUP);
  assert.equal(g.sender, OWNER_PN);
});

test("LID self chat maps to the PN form (me.lid known)", () => {
  const e = normalizeMessage(msg({ remoteJid: OWNER_LID, id: "L1", fromMe: true }, { conversation: "via lid" }), ownerCtx())!;
  assert.equal(e.chat, OWNER_PN);
});

test("LID self chat maps via a persisted LID->PN pair when me.lid is unknown", () => {
  const ctx = ctxWith({ pn: OWNER_PN, lid: null }, { [OWNER_LID]: OWNER_PN });
  assert.equal(normalizeMessage(msg({ remoteJid: OWNER_LID, id: "L2", fromMe: true }, { conversation: "x" }), ctx)!.chat, OWNER_PN);
  // ... or via remoteJidAlt on the key itself
  const bare = ctxWith({ pn: OWNER_PN, lid: null });
  assert.equal(normalizeMessage(msg({ remoteJid: OWNER_LID, remoteJidAlt: OWNER_PN, id: "L3", fromMe: true }, { conversation: "x" }), bare)!.chat, OWNER_PN);
  // other LID chats are left alone
  assert.equal(normalizeMessage(msg({ remoteJid: ALICE_LID, id: "L4" }, { conversation: "x" }), ctx)!.chat, ALICE_LID);
});

test("only the LID is known: self chat normalizes to the LID", () => {
  const ctx = ctxWith({ pn: null, lid: OWNER_LID });
  const e = normalizeMessage(msg({ remoteJid: OWNER_LID, id: "L5", fromMe: true }, { conversation: "x" }), ctx)!;
  assert.equal(e.chat, OWNER_LID);
  assert.equal(e.sender, OWNER_LID);
});

test("meFromContact handles PN id with device suffix and LID", () => {
  assert.deepEqual(meFromContact({ id: "447700900001:12@s.whatsapp.net", lid: "123456789012345:12@lid" }, fakeJids), { pn: OWNER_PN, lid: OWNER_LID });
  assert.deepEqual(meFromContact({ id: "123456789012345:12@lid", phoneNumber: OWNER_PN }, fakeJids), { pn: OWNER_PN, lid: OWNER_LID });
});

test("extractMappings reads remoteJidAlt and participantAlt", () => {
  assert.deepEqual(extractMappings(msg({ remoteJid: ALICE_LID, remoteJidAlt: ALICE_PN, id: "x" }, null), fakeJids), [{ lid: ALICE_LID, pn: ALICE_PN }]);
  assert.deepEqual(
    extractMappings(msg({ remoteJid: GROUP, participant: "447700900002:2@s.whatsapp.net", participantAlt: ALICE_LID, id: "x" }, null), fakeJids),
    [{ lid: ALICE_LID, pn: ALICE_PN }],
  );
});

test("non-chat content is dropped: status broadcast, reactions, empty", () => {
  const ctx = ownerCtx();
  assert.equal(normalizeMessage(msg({ remoteJid: "status@broadcast", id: "B1", participant: ALICE_PN }, { conversation: "story" }), ctx), null);
  assert.equal(normalizeMessage(msg({ remoteJid: ALICE_PN, id: "R1" }, { reactionMessage: {} } as never), ctx), null);
  assert.equal(normalizeMessage(msg({ remoteJid: ALICE_PN, id: "E1" }, null), ctx), null);
});

test("append upserts are ignored; notify upserts are mapped (every message in the batch)", () => {
  const messages = [
    msg({ remoteJid: ALICE_PN, id: "H1" }, { conversation: "old" }),
    msg({ remoteJid: ALICE_PN, id: "H2" }, { conversation: "older" }),
  ];
  assert.deepEqual(normalizeUpsert({ type: "append", messages }, ownerCtx()), []);
  assert.deepEqual(normalizeUpsert({ type: "notify", messages }, ownerCtx()).map((e) => e.id), ["H1", "H2"]);
});

test("ingestUpsert: append is not logged or cached but still teaches LID pairs; notify is", () => {
  const dir = tmpDir();
  const log = new EventLog(path.join(dir, "events.jsonl"));
  const lids = new LidStore(path.join(dir, "lid-map.json"));
  const cache = new MessageCache<WAMessageLike>(path.join(dir, "messages.jsonl"));
  const state: BridgeState = { connected: true, me: { pn: OWNER_PN, lid: null } };
  const d = { jids: fakeJids, state, lids, log, cache };

  const n0 = ingestUpsert(d, { type: "append", messages: [msg({ remoteJid: OWNER_LID, remoteJidAlt: OWNER_PN, id: "OLD", fromMe: true }, { conversation: "x" })] });
  assert.equal(n0, 0);
  assert.equal(log.lastSeq, 0);
  assert.equal(cache.get("OLD"), undefined);
  assert.equal(lids.pnForLid(OWNER_LID), OWNER_PN);

  // a later notify in the LID self chat (no Alt on the key) is recognized from the stored pair
  const n1 = ingestUpsert(d, { type: "notify", messages: [msg({ remoteJid: OWNER_LID, id: "NEW", fromMe: true }, { conversation: "/help" })] });
  assert.equal(n1, 1);
  assert.equal(log.read(0).events[0]!.chat, OWNER_PN);
  assert.ok(cache.get("NEW"));
  // the pair survives a restart
  assert.equal(new LidStore(path.join(dir, "lid-map.json")).pnForLid(OWNER_LID), OWNER_PN);
  // duplicate delivery is not logged twice
  assert.equal(ingestUpsert(d, { type: "notify", messages: [msg({ remoteJid: OWNER_PN, id: "NEW", fromMe: true }, { conversation: "/help" })] }), 0);
  log.close();
});
