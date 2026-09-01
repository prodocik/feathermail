# MCP security

MCP is off by default. Enabling it in **Settings → AI & MCP** provisions only
the fixed local `stdio` profile at **Read + Draft**; Send and Delete are never
implied. Unknown `FEATHERMAIL_MCP_CLIENT_ID` values are denied rather than
enrolling themselves.

The process permission and account list are ceilings only: they can reduce a
durable Core policy, never create or enlarge one. Every tool/resource action
re-reads the live in-app switch, so turning MCP off stops an already-running
stdio process from exposing account metadata without a restart.

**Settings → AI & MCP** lists at most 20 durable client-policy rows, containing
only the persisted profile name, enabled/revoked state, and permission level.
Its level selector reaches one Core transaction for enabled `stdio` only:
the selected D57 ceiling applies to every tool, removes all prior per-tool
grants, and makes pending or unconsumed Allow-once requests terminal
`invalidated`. A process `full` value cannot restore an action after the user
lowers the persistent level; the selector cannot enroll or re-enable a profile.
**Revoke access** is a Core transaction: it disables that enrolled client,
removes every per-tool grant, and terminally invalidates pending and unconsumed
Allow-once approvals. The profile remains revoked across a restart and a global
MCP off/on cycle; enabling MCP provisions `stdio` only when it does not exist.
There is no implicit re-enrolment or environment-based re-enable. Only the
revoked existing local `stdio` profile can be explicitly re-enabled while the
global switch is on, after a native GTK confirmation whose Cancel button is the
default: the Core transaction creates a fresh Draft/no-grants policy and never
changes historical terminal approvals. Revocation is linearized with
authorization and confirmation resolution: requests observed after its commit
fail closed, while an action already successfully authorized before that
transaction is not falsely claimed to be cancelled.

`send_draft`, `delete_message`, `bulk_delete`, `bulk_permanent_delete`, and `permanent_delete` create an opaque
request in Core. The GTK dialog is the only authority: **Deny**, **Allow
once**, or per-tool **Always allow**. `confirm: true` is not accepted in MCP
schemas; for those high-risk actions any supplied `confirm` field is rejected
as `INVALID_ARGUMENT` and cannot authorize anything. An Allow-once send is
bound to the current draft revision and is consumed together with the
immutable outbox snapshot, so an edited draft needs a new decision.

`bulk_delete` and `bulk_permanent_delete` are bounded to 1…100 unique non-empty
ids, but do not use the generic single-target authorization path. Core derives
a length-framed, order-independent SHA-256 fingerprint of the exact
account-bound set with a distinct capability domain, and persists only that
digest plus a safe count (`target_id` is NULL). A matching Allow-once consume
and the corresponding vector `Command::Trash` or
`Command::PermanentDelete` dispatch share one immediate SQLite transaction; a
changed set, failed target, expiry, revoke, or disabled MCP leaves both the
local mailbox/queue and the approval fail-closed. The permanent variant bypasses
Trash and has no undo ticket. GTK shows only the count, never raw ids,
account/client data, subjects, bodies, arguments or the digest.

`bulk_archive`, `bulk_mark_read`, `bulk_mark_unread`, `bulk_star`, and
`bulk_unstar` are
Draft-level W actions, not high-risk approval: their schemas and shared runtime
parser require 1…100 unique non-empty ids before the matching existing Core
vector transaction runs. `bulk_star` and `bulk_unstar` are set-only and never
infer a batch toggle from prior state. An unknown thread fails that transaction
as generic `MESSAGE_NOT_FOUND`, so it leaves no partial placement/flag or queue
change and does not reflect the submitted id in the MCP error or audit row.

`bulk_move` is a Draft-level M action with the same 1…100 bounded input. Its
destination must be an existing Custom folder in the same allowed account;
system, virtual, unlisted, and foreign folder ids are `INVALID_ARGUMENT`
before the `Command::Move` transaction. An unknown thread is the same generic
atomic `MESSAGE_NOT_FOUND` no-op, without its raw id in the response or audit.

`bulk_snooze` is a Draft-level M action with the same 1…100 bounded input and
the existing `snooze_message` integer Unix UTC `until` semantic. It runs the
same local `Command::Snooze` transaction as GTK: durable local undo receipts
are created, but no provider operation is queued. Malformed input and an
unknown thread fail atomically without raw ids in the response or audit.

`restore_message` is a Draft-level M action, but is not a raw `undo` access:
MCP supplies only account and thread identity. Core requires the current local
Trash state (overlay or discovered real Trash folder) and selects only its exact
reversible Trash lifecycle. An unknown thread is `MESSAGE_NOT_FOUND`; a known
non-trashed thread, missing lifecycle, permanent deletion, or newer
placement-changing intent is `OPERATION_NOT_SUPPORTED`. Selection and consume
share one immediate SQLite transaction, so a competing placement change cannot
make the Trash snapshot stale. Pending work is cancelled inside the existing
undo state machine; running/acked work receives only the existing causal Move.
The caller never receives or supplies the original opaque operation/ticket id,
and audit keeps the usual canonical tool/outcome metadata without thread id.

Passwords, OAuth tokens, raw keyring values, arbitrary SQL, commands and
shell execution are never MCP capabilities. Audit rows contain only a
canonical enrolled client, a static registered tool, outcome and time, plus a
Core-verified account after success; `target_id` is always `NULL`. Raw caller
identifiers, arguments and results never persist, including subjects or bodies.
MCP results are metadata-only: thread previews and message/draft bodies are
never returned; draft body text is accepted only as create/update input.
`get_message` is an account-scoped Read projection of only message id,
thread/folder identity, date, sender, subject and unread/starred/attachment
state; it excludes provider UID, raw headers, size/cache data, preview and body.

The **Activity** section reads at most 20 of those existing Core audit rows on
a worker, retaining its last safe metadata projection if that read fails. It
shows only canonical persisted client/tool, outcome, and time; it never reads
SQLite directly or renders account/target ids, arguments, results, or raw
errors.

Attachment paths are canonicalized and must remain under the explicitly
configured attachment root; traversal and symlink escapes fail closed.
