// Pure WAMessage -> Event mapping plus JID helpers.
//
// Nothing here imports Baileys. The JID helpers are injected (`JidShim`):
// production passes Baileys' own jidNormalizedUser / isLidUser / ... and
// tests pass a small stand-in, so the tests never load the package.
// JIDs are never split by hand or compared with `===` on raw strings here:
// everything goes through the shim.

/** The subset of Baileys' WABinary jid utils this bridge uses. */
export interface JidShim {
  jidNormalizedUser(jid: string | undefined): string;
  areJidsSameUser(a: string | undefined, b: string | undefined): boolean;
  isLidUser(jid: string | undefined): boolean | undefined;
  isPnUser(jid: string | undefined): boolean | undefined;
  isJidGroup(jid: string | undefined): boolean | undefined;
  isJidBroadcast(jid: string | undefined): boolean | undefined;
  isJidNewsletter(jid: string | undefined): boolean | undefined;
}

export type AttachmentKind = "voice" | "audio" | "photo" | "video" | "document";

export interface Attachment {
  /** The message id; downloads (later) are keyed by it. */
  id: string;
  kind: AttachmentKind;
  mime: string | null;
  name: string | null;
  size: number | null;
}

/** One bridge event, exactly as served by GET /events (field names are the contract). */
export interface BridgeEvent {
  seq: number;
  chat: string;
  id: string;
  fromMe: boolean;
  sender: string;
  pushName: string | null;
  text: string;
  quotedId: string | null;
  timestamp: number;
  isGroup: boolean;
  attachments: Attachment[];
}

export type UnsequencedEvent = Omit<BridgeEvent, "seq">;

export interface Me {
  pn: string | null;
  lid: string | null;
}

export interface LidPn {
  lid: string;
  pn: string;
}

/** What normalization needs to know about the owner and known LID<->PN pairs. */
export interface NormalizeCtx {
  jids: JidShim;
  me: Me;
  /** Known PN for a (normalized) LID, if any. */
  pnForLid(lid: string): string | undefined;
  /** Known LID for a (normalized) PN, if any. */
  lidForPn(pn: string): string | undefined;
}

// Minimal structural view of a Baileys WAMessage (proto.IWebMessageInfo).
// Long-typed numbers arrive as number or a Long-like object.
type LongLike = number | string | { toNumber(): number } | null | undefined;

interface ContextInfo {
  stanzaId?: string | null;
}
interface MediaBase {
  mimetype?: string | null;
  fileLength?: LongLike;
  caption?: string | null;
  contextInfo?: ContextInfo | null;
}
export interface WAMessageContentLike {
  conversation?: string | null;
  extendedTextMessage?: { text?: string | null; contextInfo?: ContextInfo | null } | null;
  imageMessage?: MediaBase | null;
  videoMessage?: MediaBase | null;
  audioMessage?: (MediaBase & { ptt?: boolean | null }) | null;
  documentMessage?: (MediaBase & { fileName?: string | null; title?: string | null }) | null;
  // wrappers that carry the real content one level down
  ephemeralMessage?: { message?: WAMessageContentLike | null } | null;
  viewOnceMessage?: { message?: WAMessageContentLike | null } | null;
  viewOnceMessageV2?: { message?: WAMessageContentLike | null } | null;
  documentWithCaptionMessage?: { message?: WAMessageContentLike | null } | null;
}
export interface WAMessageLike {
  key: {
    remoteJid?: string | null;
    remoteJidAlt?: string | null;
    fromMe?: boolean | null;
    id?: string | null;
    participant?: string | null;
    participantAlt?: string | null;
  };
  message?: WAMessageContentLike | null;
  messageTimestamp?: LongLike;
  pushName?: string | null;
}

export interface UpsertLike {
  type: string;
  messages: WAMessageLike[];
}

// ---------------------------------------------------------------- JID helpers

/** Normalized form of the owner's own chat: PN when known, else LID. */
export function selfJid(ctx: NormalizeCtx): string | null {
  return ctx.me.pn ?? ctx.me.lid;
}

function sameKind(j: JidShim, a: string, b: string): boolean {
  return Boolean(j.isLidUser(a)) === Boolean(j.isLidUser(b));
}

/** Same user and same addressing space (LID vs PN) according to the shim. */
export function sameUser(j: JidShim, a: string | null | undefined, b: string | null | undefined): boolean {
  if (!a || !b) return false;
  return j.areJidsSameUser(a, b) && sameKind(j, a, b);
}

/** Is this JID (either form) the owner? */
export function isOwner(jid: string | null | undefined, ctx: NormalizeCtx): boolean {
  if (!jid) return false;
  const j = ctx.jids;
  const n = j.jidNormalizedUser(jid);
  if (!n) return false;
  if (sameUser(j, n, ctx.me.pn) || sameUser(j, n, ctx.me.lid)) return true;
  if (j.isLidUser(n)) {
    const pn = ctx.pnForLid(n);
    return Boolean(pn && sameUser(j, pn, ctx.me.pn));
  }
  if (j.isPnUser(n)) {
    const lid = ctx.lidForPn(n);
    return Boolean(lid && sameUser(j, lid, ctx.me.lid));
  }
  return false;
}

/** Normalize a user/chat JID; the owner (in either form) becomes the stable self JID. */
export function canonical(jid: string | null | undefined, ctx: NormalizeCtx, alt?: string | null): string {
  const n = ctx.jids.jidNormalizedUser(jid ?? undefined);
  if (isOwner(n, ctx) || (alt && isOwner(alt, ctx))) return selfJid(ctx) ?? n;
  return n;
}

/** LID<->PN pairs carried on a message key (remoteJidAlt / participantAlt). */
export function extractMappings(msg: WAMessageLike, j: JidShim): LidPn[] {
  const out: LidPn[] = [];
  const pair = (a?: string | null, b?: string | null) => {
    if (!a || !b) return;
    const na = j.jidNormalizedUser(a);
    const nb = j.jidNormalizedUser(b);
    if (j.isLidUser(na) && j.isPnUser(nb)) out.push({ lid: na, pn: nb });
    else if (j.isPnUser(na) && j.isLidUser(nb)) out.push({ lid: nb, pn: na });
  };
  pair(msg.key.remoteJid, msg.key.remoteJidAlt);
  pair(msg.key.participant, msg.key.participantAlt);
  return out;
}

/**
 * The owner's own JIDs from Baileys' creds.me / sock.user (a Contact whose
 * `id` may be PN or LID, with device suffix), normalized.
 */
export function meFromContact(
  c: { id?: string | null; lid?: string | null; phoneNumber?: string | null } | null | undefined,
  j: JidShim,
): Me {
  const me: Me = { pn: null, lid: null };
  for (const raw of [c?.id, c?.lid, c?.phoneNumber]) {
    if (!raw) continue;
    const n = j.jidNormalizedUser(raw);
    if (!n) continue;
    if (j.isLidUser(n)) me.lid ??= n;
    else if (j.isPnUser(n)) me.pn ??= n;
  }
  return me;
}

// ------------------------------------------------------------ content mapping

function toNum(v: LongLike): number | null {
  if (v === null || v === undefined) return null;
  if (typeof v === "number") return Number.isFinite(v) ? v : null;
  if (typeof v === "string") {
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  }
  if (typeof v.toNumber === "function") return v.toNumber();
  return null;
}

/** Peel ephemeral / view-once / document-with-caption wrappers. */
export function unwrap(m: WAMessageContentLike | null | undefined): WAMessageContentLike | null {
  let cur = m ?? null;
  for (let i = 0; cur && i < 5; i++) {
    const inner =
      cur.ephemeralMessage?.message ??
      cur.viewOnceMessage?.message ??
      cur.viewOnceMessageV2?.message ??
      cur.documentWithCaptionMessage?.message;
    if (!inner) break;
    cur = inner;
  }
  return cur;
}

function textOf(c: WAMessageContentLike): string {
  return (
    c.conversation ??
    c.extendedTextMessage?.text ??
    c.imageMessage?.caption ??
    c.videoMessage?.caption ??
    c.documentMessage?.caption ??
    ""
  );
}

function quotedIdOf(c: WAMessageContentLike): string | null {
  const ci =
    c.extendedTextMessage?.contextInfo ??
    c.imageMessage?.contextInfo ??
    c.videoMessage?.contextInfo ??
    c.audioMessage?.contextInfo ??
    c.documentMessage?.contextInfo;
  return ci?.stanzaId || null;
}

function attachmentsOf(c: WAMessageContentLike, id: string): Attachment[] {
  const out: Attachment[] = [];
  const add = (kind: AttachmentKind, m: MediaBase, name: string | null = null) =>
    out.push({ id, kind, mime: m.mimetype ?? null, name, size: toNum(m.fileLength) });
  if (c.imageMessage) add("photo", c.imageMessage);
  if (c.videoMessage) add("video", c.videoMessage);
  if (c.audioMessage) add(c.audioMessage.ptt ? "voice" : "audio", c.audioMessage);
  if (c.documentMessage) add("document", c.documentMessage, c.documentMessage.fileName ?? c.documentMessage.title ?? null);
  return out;
}

/**
 * Map one WAMessage to an event (without seq). Returns null for things that
 * are not chat messages: no key/id, status broadcasts, newsletters, or
 * content with neither text nor a supported attachment (reactions, protocol
 * and stub messages, stickers, polls, ...).
 */
export function normalizeMessage(msg: WAMessageLike, ctx: NormalizeCtx): UnsequencedEvent | null {
  const j = ctx.jids;
  const { remoteJid, id } = msg.key;
  if (!remoteJid || !id) return null;
  const raw = j.jidNormalizedUser(remoteJid);
  if (!raw || j.isJidBroadcast(raw) || j.isJidNewsletter(raw)) return null;
  const content = unwrap(msg.message);
  if (!content) return null;
  const text = textOf(content);
  const attachments = attachmentsOf(content, id);
  if (!text && attachments.length === 0) return null;

  const isGroup = Boolean(j.isJidGroup(raw));
  const chat = isGroup ? raw : canonical(raw, ctx, msg.key.remoteJidAlt);
  const fromMe = Boolean(msg.key.fromMe);
  let sender: string;
  if (fromMe) sender = selfJid(ctx) ?? chat;
  else if (isGroup) sender = canonical(msg.key.participant ?? "", ctx, msg.key.participantAlt) || chat;
  else sender = chat;

  return {
    chat,
    id,
    fromMe,
    sender,
    pushName: msg.pushName ?? null,
    text,
    quotedId: quotedIdOf(content),
    timestamp: toNum(msg.messageTimestamp) ?? Math.floor(Date.now() / 1000),
    isGroup,
    attachments,
  };
}

/** Only `notify` upserts are real-time messages; `append` (history/backfill) is ignored. */
export function normalizeUpsert(up: UpsertLike, ctx: NormalizeCtx): UnsequencedEvent[] {
  if (up.type !== "notify") return [];
  const out: UnsequencedEvent[] = [];
  for (const m of up.messages) {
    const e = normalizeMessage(m, ctx);
    if (e) out.push(e);
  }
  return out;
}
