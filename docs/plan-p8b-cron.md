# P8b — cron expressions for scheduled tasks

Status: COMPLETE (2026-09-20)

113 tests pass (8 cron unit + 1 daemon e2e), and the untested-in-CI firing path
was checked live: a `* * * * *` heartbeat set over the wire reported `nextDueMs`
exactly 50.0 s out (the next minute boundary), the client disconnected, and the
journal shows exactly one tick fired while detached — with the following due time
on the boundary after that, confirming re-arm computes the next *occurrence*
rather than `now + 60`.

## The gap

Residents made schedules survive their client, but only *interval* schedules
exist: `everyMs` and `inMs`. The roadmap line said "cron schedules +
daemon-resident sessions"; the second half is done, this is the first.

## Scope

A five-field Vixie-style expression (`min hour dom month dow`) supporting `*`,
`N`, `A-B`, lists, and steps (`*/2`, `5/15`, `1-30/5`), with the standard
day-of-month OR day-of-week rule when both are restricted. No month/day names
(`JAN`, `MON`) — the numeric forms cover a personal harness's needs and the
parser stays small. **Local time, not UTC**: `0 9 * * 1-5` means *my* 09:00; a
schedule that silently runs at 03:00 would be worse than none. DST is whatever
the platform reports for each queried instant.

## Shape

- `hasp-core/src/cron.rs`: pure parser + matcher (bitset fields), plus
  `next_due` — a minute-stepped search bounded at 366 days, returning `None`
  for schedules with no occurrence in that window (e.g. Feb 29 far away), which
  the runtime treats as "not due".
- The time math is split so tests are timezone-independent: `next_due_with_offset`
  is pure (UTC + a fixed offset) and unit-tested; the live path asks the
  platform via `localtime_r` (`libc`, already in the tree through tokio/mio).
- `Entry::Heartbeat` and `HeartbeatState` gain `cron: Option<String>`
  (`#[serde(default)]`, so old journals still load). `append_heartbeat` takes it.
- The daemon validates at `heartbeat/set` time — an invalid expression is an
  error reply, never a journaled dud — and re-arms cron ticks by computing the
  *next occurrence*, not `now + every`. `heartbeat/list` reports `cron` and the
  runtime's `nextDueMs`.
- TUI: `/heartbeat cron <expr> <instruction>` (five space-separated fields, no
  quoting needed since the expression has exactly five tokens).

## Not tested end-to-end

A cron tick firing takes at least a minute of wall clock, which would
quadruple the daemon suite's runtime; firing shares its entire path with
`everyMs` (claim-before-deliver, coalescing, resident loop), which *is* covered.
What gets tested: parsing, the DOM/DOW rule, `next_due` arithmetic at a fixed
offset, the 366-day bound, set-time validation over the wire, and the list
payload. That trade is recorded here rather than hidden.
