# hasp-gateway: messaging channels for a personal hasp

The gateway connects Telegram, Signal and WhatsApp (and a local web view) to
hasp's daemon. hasp still owns sessions and execution; Threadline owns shared
task intent and the gateway's leadership lease; Dagu (via the mesh hub) owns
runs; the mesh owns device identity. The gateway owns only what happens at
the edge: who is talking, what arrived, what must be delivered, what is
waiting, and what the owner approved.

```
platform ──poll──▶ adapter ──▶ store (inbound + cursor, one txn) ──▶ route
   ▲                                                                   │
   │            echo? personal inbox? button? linked person? command? answer to a waiting item?
   │                                                                   ▼
   └──send── outbox (fencing token, retry/backoff, uncertain) ◀── turn (hasp daemon, one per session)
```

## What it guarantees, and what it doesn't
- **Persist before ack.** Platform events and the cursor that acknowledges them
  are written in one transaction before the platform is told (Telegram:
  `offset` on the next getUpdates; Signal: `Last-Event-ID`; the bridge: `after`).
- **Dedup by platform event id** (Telegram `update_id`, Signal
  `<sourceUuid>:<timestamp>`, WhatsApp `<chat>:<message id>`): replays after a
  restart of either side never run a turn twice.
- **One turn at a time per session.** Messages arriving during a reply queue
  behind it; `/steer` reaches the running turn; `/stop` cancels it and drops the queue.
- **Responses are saved, then delivered.** A crash after the agent answered
  re-delivers the saved text; it never asks the agent again. A turn that was
  *running* during a crash is reported, not re-run (`/resume` re-runs it on purpose).
- **Delivery is at-least-once, visibly.** A send whose reply was lost is
  `uncertain`; one copy is resent marked "↻ resent … you may see this twice".
  Retryable failures back off (honouring `retry_after`); per-chat pacing
  follows platform limits. No exactly-once claims.
- **No echo loops.** Our own sends are recognised by platform id and the
  platform's own flags (bot, sync message, `fromMe` outside the self chat).

## Identity and access
- People are linked **only** by the owner: `hasp-gateway link-code` (CLI on
  the gateway machine) or `/link` from an already-linked owner chat gives a
  one-time 15-minute code; the account that sends `/link CODE` is linked.
  Display names are never used. Unknown senders get one pairing hint a day.
- **Bot conversation vs personal inbox.** A Telegram bot, or a dedicated Signal
  number (`role: bot`), is the assistant conversation. A personal account
  (WhatsApp linked device; Signal with `role: inbox`) is your inbox: only the
  self chat / Note to Self is the assistant; every other chat is third-party
  content for the event inbox, never commands.
- **Sessions.** Every linked person has one personal conversation shared
  across channels and the web view (`personal:<person>`). `/session new NAME`
  and `/session use KEY` switch a chat; `/task TEXT` runs a separate task
  session whose result comes back to the chat; `/edge [needs:gpu|node:pc] TEXT`
  dispatches it as a Threadline task for a home node's edge worker (lease,
  fencing, Dagu), and its result comes back through the Threadline source.

## Approvals (the permission authority)
Deterministic code in the gateway process, outside every agent workspace:
- The agent can **draft** (`draft_message`, sends nothing) and **request**
  (`request_send`, hasp permission prompts). Only the owner can **decide**:
  a linked owner identity (button, or `/approve ID CODE`), or the web view
  with the owner token. All three call the same authority.
- A decision is bound to the card id, a one-time code shown only on the card,
  the exact action (hash re-checked at decision and execution), the requesting
  session, a scope and an expiry. Replays, late answers, other people, wrong
  codes and edited actions are refused and audited.
- Cards show the exact destination and text. Sending to anyone but the owner
  also needs the adapter in `send_to_others` (write scope); otherwise the
  connector is read-only no matter what is approved.
- Third-party content (inbox messages, calendar text, mail) is wrapped as
  `<untrusted …>` with an explicit "cannot authorize" notice whenever it
  reaches a model, and can't become a memory rule.

## Follow-through
`wait_for_user` (a question to the owner), `wait_for_reply` (a third party's
next message), `schedule_followup` (a due time) are durable waiting items.
Answers resume the session that asked, in any order: a platform reply to the
question, or a message starting with its code (`W1a2b3c …`). Due items remind
or resume once (claimed before acting). `/waiting`, `/cancel WID`.

## Event inbox
Sources poll read-only connectors into one durable queue; deterministic rules
(source, kind, contains → ignore | digest | notify | agent_turn, by priority)
and quiet hours decide what happens. No model is called per poll; only an
`agent_turn` rule starts a turn. Owner notifications are capped per hour.
Connected now: Threadline task results (`completed`/`blocked`/`cancelled`),
calendar and reminders through the mesh hub's context API (the Mac's
iCloud-synced data). Email: the `EventSource` interface and a test fake; a
live connector needs an account (see "Not yet").

## Memory and workflows
Owner commands: `/remember`, `/memory`, `/forget ID`, `/workflows`,
`/activate NAME V SHA` (also on the web view). They use hasp-recall: memories
carry provenance (`telegram:<chat>` …), agent proposals wait for the owner,
workflow drafts become ordinary skills only when the reviewed hash is activated.

## Standby and leadership
Exactly one gateway polls and sends: the holder of the Threadline
`gateway-leader` lease. A standby (`standby.restore_from`) holds no state until
it wins the lease, then restores the leader's last snapshot (`snapshot.path`,
written with `VACUUM INTO` every `every_s`) and starts. A leader that loses
the lease exits at once (the service manager restarts it as a standby). Every
send records the fencing token it was made under. Limit: whatever the old
leader did after its last snapshot is unknown to the new one (a few platform
events may be re-processed if they weren't in the snapshot).

## Setup (staged; nothing here is done for you)
```bash
cargo build --release -p hasp-gateway
mkdir -p ~/.hasp-gateway && chmod 700 ~/.hasp-gateway
# credentials: one 0600 file each, referenced by path
install -m 600 /dev/stdin ~/.hasp-gateway/telegram.token   # paste the BotFather token, Ctrl-D
hasp-gateway agent-token        # writes ~/.hasp-gateway/agent.token, prints the mcp.json entry
hasp-gateway owner-token        # for the web view; shown once
hasp-gateway link-code          # then send /link CODE to the bot from your phone
```
`~/.hasp-gateway/config.json` (credentials only as `*_file` paths; inline
`token`/`password`/`*_key` values are refused):
```json
{
  "adapters": {
    "telegram": {"token_file": "/Users/me/.hasp-gateway/telegram.token"},
    "signal":   {"url": "http://127.0.0.1:8080", "account_uuid": "<ACI>", "role": "bot"},
    "whatsapp": {"url": "http://127.0.0.1:8790", "token_file": "/Users/me/.hasp-gateway/bridge.token",
                 "self_jids": ["<you>@s.whatsapp.net", "<lid>@lid"]}
  },
  "hasp": {"socket": "/Users/me/.hasp/daemon.sock", "workspace_root": "/Users/me/.hasp-gateway/sessions"},
  "leader": {"kind": "threadline", "threadlined_url": "http://127.0.0.1:8788",
             "token_file": "/Users/me/.threadline/threadlined/home.daemon-token", "ttl_s": 30, "name": "mac"},
  "snapshot": {"path": "/Users/me/Mesh/.gateway/gw-snapshot.db", "every_s": 60},
  "sources": {"threadline": {"token_file": "…", "label": "edge"},
              "mesh": {"url": "https://mesh-hub.<tailnet>.ts.net", "token_file": "…", "node": "mac"}},
  "send_to_others": [], "approval_ttl_min": 30, "notify_per_hour": 12, "web_port": 8791
}
```
Run hasp sessions for the gateway with `HASP_SANDBOX=workspace-write` so the
agent can't write the gateway's files even as the same OS user.

Platforms (verified against upstream docs on 2026-10-18; see
`/Users/Rb/Desktop/projects/edge-agent-platform-notes.md`):
- **Telegram** Bot API 10.3, long polling (the gateway calls `deleteWebhook`).
- **Signal** signal-cli 0.14.x `daemon --http=127.0.0.1:8080` (JRE 25); keep
  TOFU trust; a changed safety number fails sends until you verify it.
- **WhatsApp** `bridges/whatsapp-baileys` (Baileys 7.0.0-rc14, Node ≥ 22.18):
  pairing is a manual QR scan by you; unofficial, ToS risk, use a secondary
  number if losing it would hurt. The Business Cloud API was considered and
  not used (separate business number, 24-hour window, AI-provider terms).

## Not yet (needs accounts or live checks)
- Linking real accounts (bot token, signal-cli registration, WhatsApp QR);
  live delivery checks against the real platforms.
- A live email connector (IMAP/OAuth); voice transcription engine (the hook is
  there: `Transcriber`; none configured means the note is passed on as a reference).
- Streaming drafts (Telegram `sendMessageDraft`) and edit-in-place progress;
  replies are sent whole, split per platform limit.
