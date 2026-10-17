// Test doubles. Nothing here (or in any test) imports Baileys: the JID shim
// below mirrors the behaviour of Baileys' jid utils closely enough for tests.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import type { JidShim, Me, NormalizeCtx, WAMessageLike } from "../src/normalize.ts";

function decode(jid: string | undefined): { user: string; server: string } | undefined {
  if (!jid) return undefined;
  const at = jid.indexOf("@");
  if (at < 0) return undefined;
  const server = jid.slice(at + 1);
  const user = jid.slice(0, at).split(":")[0]!.split("_")[0]!;
  return { user, server };
}

export const fakeJids: JidShim = {
  jidNormalizedUser(jid) {
    const d = decode(jid);
    if (!d) return "";
    return `${d.user}@${d.server === "c.us" ? "s.whatsapp.net" : d.server}`;
  },
  areJidsSameUser(a, b) {
    return decode(a)?.user === decode(b)?.user;
  },
  isLidUser: (j) => j?.endsWith("@lid"),
  isPnUser: (j) => j?.endsWith("@s.whatsapp.net"),
  isJidGroup: (j) => j?.endsWith("@g.us"),
  isJidBroadcast: (j) => j?.endsWith("@broadcast"),
  isJidNewsletter: (j) => j?.endsWith("@newsletter"),
};

export const OWNER_PN = "447700900001@s.whatsapp.net";
export const OWNER_LID = "123456789012345@lid";
export const ALICE_PN = "447700900002@s.whatsapp.net";
export const ALICE_LID = "222222222222222@lid";
export const BOB_LID = "333333333333333@lid";
export const GROUP = "120363000000000001@g.us";

export function ctxWith(me: Me, map: Record<string, string> = {}): NormalizeCtx {
  const rev = Object.fromEntries(Object.entries(map).map(([l, p]) => [p, l]));
  return { jids: fakeJids, me, pnForLid: (l) => map[l], lidForPn: (p) => rev[p] };
}

export const ownerCtx = () => ctxWith({ pn: OWNER_PN, lid: OWNER_LID });

export function tmpDir(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "wa-bridge-test-"));
}

/** A Long-like value as protobufjs produces for fileLength / messageTimestamp. */
export const long = (n: number) => ({ toNumber: () => n });

export function msg(
  key: WAMessageLike["key"],
  message: WAMessageLike["message"],
  extra: Partial<WAMessageLike> = {},
): WAMessageLike {
  return { key, message, messageTimestamp: 1760780000, pushName: "Someone", ...extra };
}
