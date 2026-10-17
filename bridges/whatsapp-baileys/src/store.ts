// Small persistent stores next to the event log:
// - LidStore: LID <-> PN pairs (JSON file, rewritten atomically on change).
// - MessageCache: bounded cache of recent raw WAMessages by key.id, backed by
//   a JSONL file. Backs Baileys' getMessage (retry re-encryption) and quoting
//   on /send. It is a cache: unreadable lines are dropped, not fatal.

import fs from "node:fs";
import type { JidShim, LidPn, Me, NormalizeCtx } from "./normalize.ts";

function writeAtomic(path: string, data: string): void {
  const tmp = `${path}.tmp-${process.pid}`;
  fs.writeFileSync(tmp, data, { mode: 0o600 });
  fs.renameSync(tmp, path);
}

export class LidStore {
  private byLid = new Map<string, string>();
  private byPn = new Map<string, string>();
  private readonly path: string;

  constructor(path: string) {
    this.path = path;
    let raw: string | null = null;
    try {
      raw = fs.readFileSync(path, "utf8");
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code !== "ENOENT") throw e;
    }
    if (raw) {
      const obj = JSON.parse(raw) as Record<string, string>;
      for (const [lid, pn] of Object.entries(obj)) this.setMem(lid, pn);
    }
  }

  private setMem(lid: string, pn: string): boolean {
    if (this.byLid.get(lid) === pn) return false;
    const oldPn = this.byLid.get(lid);
    if (oldPn !== undefined) this.byPn.delete(oldPn);
    this.byLid.set(lid, pn);
    this.byPn.set(pn, lid);
    return true;
  }

  /** Record pairs (already normalized); persists if anything changed. */
  add(pairs: LidPn[]): boolean {
    let changed = false;
    for (const p of pairs) if (p.lid && p.pn && this.setMem(p.lid, p.pn)) changed = true;
    if (changed) writeAtomic(this.path, JSON.stringify(Object.fromEntries(this.byLid), null, 1));
    return changed;
  }

  pnForLid(lid: string): string | undefined {
    return this.byLid.get(lid);
  }

  lidForPn(pn: string): string | undefined {
    return this.byPn.get(pn);
  }
}

/** Mutable connection state shared between the socket wiring and the HTTP server. */
export interface BridgeState {
  connected: boolean;
  me: Me;
}

export function makeCtx(jids: JidShim, state: BridgeState, lids: LidStore): NormalizeCtx {
  return {
    jids,
    get me() {
      return state.me;
    },
    pnForLid: (l) => lids.pnForLid(l),
    lidForPn: (p) => lids.lidForPn(p),
  };
}

export interface CacheCodec {
  replacer?: (key: string, value: unknown) => unknown;
  reviver?: (key: string, value: unknown) => unknown;
}

export class MessageCache<M extends { key?: { id?: string | null } | null }> {
  private map = new Map<string, M>();
  private lines = 0;
  private readonly path: string;
  private readonly max: number;
  private readonly codec: CacheCodec;

  constructor(path: string, max = 2000, codec: CacheCodec = {}) {
    this.path = path;
    this.max = max;
    this.codec = codec;
    let raw = "";
    try {
      raw = fs.readFileSync(path, "utf8");
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code !== "ENOENT") throw e;
    }
    for (const line of raw.split("\n")) {
      if (!line) continue;
      try {
        const m = JSON.parse(line, codec.reviver as never) as M;
        if (m?.key?.id) this.remember(m.key.id, m);
      } catch {
        /* cache: drop unreadable lines */
      }
    }
    this.compact();
  }

  private remember(id: string, m: M): void {
    this.map.delete(id);
    this.map.set(id, m);
    while (this.map.size > this.max) this.map.delete(this.map.keys().next().value as string);
  }

  private compact(): void {
    const body = [...this.map.values()].map((m) => JSON.stringify(m, this.codec.replacer as never) + "\n").join("");
    writeAtomic(this.path, body);
    this.lines = this.map.size;
  }

  put(m: M): void {
    const id = m?.key?.id;
    if (!id) return;
    this.remember(id, m);
    fs.appendFileSync(this.path, JSON.stringify(m, this.codec.replacer as never) + "\n", { mode: 0o600 });
    if (++this.lines > this.max * 2) this.compact();
  }

  get(id: string): M | undefined {
    return this.map.get(id);
  }

  get size(): number {
    return this.map.size;
  }
}
