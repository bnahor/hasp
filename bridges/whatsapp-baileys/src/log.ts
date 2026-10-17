// Append-only JSONL event log with a monotonically increasing seq.
//
// - Every event is written and fsynced before it becomes visible to readers.
// - (chat, id) is deduplicated; the set is rebuilt from the file on open.
// - A torn final line (bytes after the last "\n") is moved to
//   `<log>.torn-<offset>` and the file is truncated to the valid prefix.
// - A corrupt line anywhere before that is an error: nothing is skipped.

import fs from "node:fs";
import type { BridgeEvent, UnsequencedEvent } from "./normalize.ts";

export class LogCorruptError extends Error {}

export interface ReadResult {
  events: BridgeEvent[];
  next: number;
}

interface Waiter {
  check(): void;
  cancel(): void;
}

const dedupKey = (chat: string, id: string) => `${chat}\u0000${id}`;

function validEvent(v: unknown): v is BridgeEvent {
  if (!v || typeof v !== "object") return false;
  const e = v as Record<string, unknown>;
  return (
    Number.isSafeInteger(e.seq) &&
    (e.seq as number) > 0 &&
    typeof e.chat === "string" &&
    typeof e.id === "string" &&
    Array.isArray(e.attachments)
  );
}

export class EventLog {
  readonly path: string;
  private fd: number;
  private events: BridgeEvent[] = [];
  private seen = new Set<string>();
  private waiters = new Set<Waiter>();
  /** Set when a torn tail was recovered on open (path of the moved bytes). */
  readonly tornFile: string | null = null;

  constructor(path: string) {
    this.path = path;
    let buf: Buffer;
    try {
      buf = fs.readFileSync(path);
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code !== "ENOENT") throw e;
      buf = Buffer.alloc(0);
    }
    const lastNl = buf.lastIndexOf(0x0a);
    const validEnd = lastNl + 1; // 0 when there is no newline at all
    if (validEnd < buf.length) {
      const torn = `${path}.torn-${validEnd}`;
      fs.writeFileSync(torn, buf.subarray(validEnd), { mode: 0o600 });
      fs.truncateSync(path, validEnd);
      this.tornFile = torn;
    }
    const body = buf.subarray(0, validEnd).toString("utf8");
    const lines = body.split("\n");
    lines.pop(); // body is "" or ends with "\n": the last piece is always ""
    let lineNo = 0;
    let prev = 0;
    for (const line of lines) {
      lineNo++;
      if (line === "") throw new LogCorruptError(`${path}:${lineNo}: empty line`);
      let v: unknown;
      try {
        v = JSON.parse(line);
      } catch {
        throw new LogCorruptError(`${path}:${lineNo}: not valid JSON`);
      }
      if (!validEvent(v)) throw new LogCorruptError(`${path}:${lineNo}: not an event`);
      if (v.seq <= prev) throw new LogCorruptError(`${path}:${lineNo}: seq ${v.seq} does not increase`);
      prev = v.seq;
      this.events.push(v);
      this.seen.add(dedupKey(v.chat, v.id));
    }
    this.fd = fs.openSync(path, "a", 0o600);
  }

  get lastSeq(): number {
    return this.events.length ? this.events[this.events.length - 1]!.seq : 0;
  }

  has(chat: string, id: string): boolean {
    return this.seen.has(dedupKey(chat, id));
  }

  /**
   * Persist one event and make it visible. Returns null for a duplicate
   * (chat, id) without assigning a seq.
   */
  append(e: UnsequencedEvent): BridgeEvent | null {
    const k = dedupKey(e.chat, e.id);
    if (this.seen.has(k)) return null;
    const ev: BridgeEvent = { seq: this.lastSeq + 1, ...e };
    const line = Buffer.from(JSON.stringify(ev) + "\n", "utf8");
    let off = 0;
    while (off < line.length) off += fs.writeSync(this.fd, line, off);
    fs.fsyncSync(this.fd);
    this.events.push(ev);
    this.seen.add(k);
    for (const w of [...this.waiters]) w.check();
    return ev;
  }

  /** Events with seq > after, oldest first, at most `limit`. */
  read(after: number, limit = 500): ReadResult {
    // binary search for the first seq > after
    let lo = 0;
    let hi = this.events.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (this.events[mid]!.seq <= after) lo = mid + 1;
      else hi = mid;
    }
    const events = this.events.slice(lo, lo + limit);
    return { events, next: events.length ? events[events.length - 1]!.seq : after };
  }

  /**
   * Resolve once an event with seq > after exists, or after `ms`, or when
   * `signal` aborts. Resolves true if events are available.
   */
  waitFor(after: number, ms: number, signal?: AbortSignal): Promise<boolean> {
    if (this.lastSeq > after) return Promise.resolve(true);
    if (ms <= 0 || signal?.aborted) return Promise.resolve(false);
    return new Promise((resolve) => {
      const done = (ok: boolean) => {
        clearTimeout(timer);
        this.waiters.delete(waiter);
        signal?.removeEventListener("abort", onAbort);
        resolve(ok);
      };
      const waiter: Waiter = {
        check: () => {
          if (this.lastSeq > after) done(true);
        },
        cancel: () => done(false),
      };
      const onAbort = () => done(false);
      const timer = setTimeout(() => done(false), ms);
      this.waiters.add(waiter);
      signal?.addEventListener("abort", onAbort, { once: true });
    });
  }

  close(): void {
    for (const w of [...this.waiters]) w.cancel();
    try {
      fs.closeSync(this.fd);
    } catch {
      /* already closed */
    }
  }
}
