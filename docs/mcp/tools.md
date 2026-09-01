# MCP tools

Every tool has an object JSON Schema returned by `tools/list`.

- Accounts/folders: `list_accounts`, `get_account`, `get_account_status`, `sync_account`,
  `list_folders`, `get_folder`, `get_folder_message_count`, `create_folder`,
  `rename_folder`, `delete_folder`.
- Mail: `list_threads`, `get_thread`, `list_thread_messages`, `get_message`, `get_messages`, `search_mail`,
  `list_snoozed`, `archive_message`, `mark_read`, `mark_unread`, `star_message`,
  `unstar_message`, `bulk_archive`, `bulk_mark_read`, `bulk_star`, `bulk_unstar`, `delete_message`,
  `bulk_delete`, `bulk_permanent_delete`, `permanent_delete`, `restore_message`, `bulk_mark_unread`, `bulk_move`, `snooze_message`,
  `bulk_snooze`, `unsnooze_message`, `move_message`.
- Drafts/send: `list_drafts`, `get_draft`, `create_draft`, `update_draft`,
  `delete_draft`, `reply_to_thread`, `forward_message`, `send_draft`, `send_email`.
- Incoming attachments: `list_attachments`, `get_attachment`,
  `download_attachment`, `save_attachment`.
- Draft attachments: `list_draft_attachments`, `attach_file_to_draft`,
  `remove_attachment_from_draft`.

## Input and result contract

`tools/list` is the machine-readable source for the advertised schemas; this
table is the compact human reference to those exact inputs. `tools/call`
checks the fields a tool needs, but does **not** globally runtime-validate
every advertised JSON Schema or promise to reject unrelated extra properties.
`get_account`, `list_folders` and `list_drafts` advertise
`additionalProperties: true`; every other current tool schema advertises
`false`. Clients should send only the fields listed below.

All required string inputs must be non-empty. `after` is exactly
`{date: integer, id: non-empty string}`; `limit` is optional and defaults to
50, but if present it must be an integer in 1…200 -- anything else (0, 201,
negative, fractional, non-numeric) is `INVALID_ARGUMENT`, not clamped. `?`
means optional. Results are metadata-only: no thread preview or message/draft
body is ever returned.

| Tool | Inputs | Result / side effect | Level | Error boundary |
|---|---|---|---|---|
| `list_accounts` | — | `accounts` metadata list | Read | access |
| `get_account` | `account_id` | one account metadata object | Read | access, account |
| `get_account_status` | `account_id` | `account_id`, persisted `status` | Read | access, account |
| `sync_account` | `account_id` | records one pending sync for the account; `queued`, `account_id` | Draft | access, account |
| `list_folders` | `account_id` | folder metadata with unread/total | Read | access, account |
| `get_folder_message_count` | `account_id`, `folder_id` | folder id, unread, total | Read | access, account/folder |
| `list_threads` | `account_id`, `folder_id`; `after?`, `limit?` | threads, total, `next?` | Read | access, account/folder/cursor |
| `list_snoozed` | `account_id`; `after?`, `limit?` | snoozed threads, total, `next?` | Read | access, account/cursor |
| `get_thread` | `account_id`, `thread_id` | thread and message metadata | Read | access, account/thread |
| `list_thread_messages` | `account_id`, `thread_id` | message metadata list | Read | access, account/thread |
| `get_message` | `account_id`, `message_id` | one message header/state metadata with thread/folder identity | Read | access, account/message |
| `search_mail` | `account_id`, `query`; `after?`, `limit?` | threads, pending-index count, `next?` | Read | access, account/query/cursor |
| `archive_message` | `account_id`, `thread_id` | queues archive operation id | Draft (W) | access, account/thread |
| `bulk_archive` | `account_id`, 1…100 unique non-empty `thread_ids` | atomically queues archive operations | Draft (W) | access, account/threads |
| `mark_read` | `account_id`, `thread_id` | queues read operation id | Draft (W) | access, account/thread |
| `bulk_mark_read` | `account_id`, 1…100 unique non-empty `thread_ids` | atomically queues read operations | Draft (W) | access, account/threads |
| `mark_unread` | `account_id`, `thread_id` | queues unread operation id | Draft (W) | access, account/thread |
| `bulk_mark_unread` | `account_id`, 1…100 unique non-empty `thread_ids` | atomically queues unread operations | Draft (W) | access, account/threads |
| `star_message` | `account_id`, `thread_id` | queues star operation id | Draft (W) | access, account/thread |
| `unstar_message` | `account_id`, `thread_id` | queues unstar operation id | Draft (W) | access, account/thread |
| `bulk_star` | `account_id`, 1…100 unique non-empty `thread_ids` | atomically queues set-star operations | Draft (W) | access, account/threads |
| `bulk_unstar` | `account_id`, 1…100 unique non-empty `thread_ids` | atomically queues set-unstar operations | Draft (W) | access, account/threads |
| `delete_message` | `account_id`, `thread_id` | queues Trash operation after approval | Full (H) | access, account/thread, GTK approval |
| `bulk_delete` | `account_id`, 1…100 unique non-empty `thread_ids` | atomically queues Trash operations after exact-batch approval | Full (H) | access, account/threads, GTK approval |
| `bulk_permanent_delete` | `account_id`, 1…100 unique non-empty `thread_ids` | atomically queues irreversible delete operations after exact-batch approval; no undo | Full (H) | access, account/threads, GTK approval |
| `permanent_delete` | `account_id`, `thread_id` | queues irreversible delete after approval | Full (H) | access, account/thread, GTK approval |
| `restore_message` | `account_id`, `thread_id` | restores only current exact Trash lifecycle; causal reverse operation only if already sent | Draft (M) | access, account/current Trash lifecycle |
| `snooze_message` | `account_id`, `thread_id`, `until` integer | records a local snooze and returns its durable operation id | Draft (M) | access, account/thread |
| `bulk_snooze` | `account_id`, 1…100 unique non-empty `thread_ids`, Unix UTC `until` integer | atomically records local snoozes | Draft (M) | access, account/threads |
| `unsnooze_message` | `account_id`, `thread_id` | `unsnoozed` (false when it was not snoozed), `queued: false` | Draft (M) | access, account/thread |
| `move_message` | `account_id`, `thread_id`, custom `folder_id` | queues move operation id | Draft (M) | access, account/thread/folder |
| `bulk_move` | `account_id`, custom `folder_id`, 1…100 unique non-empty `thread_ids` | atomically queues move operations | Draft (M) | access, account/threads/folder |
| `create_folder` | `account_id`, `name` | new `folder_id`, queued create | Draft (M) | access, account/name |
| `rename_folder` | `account_id`, `folder_id`, `name` | `folder_id`, `queued` (false when the name already matched) | Draft (M) | access, account/folder/name |
| `delete_folder` | `account_id`, `folder_id` | `folder_id`, `deleted`, `queued` (false when the folder never reached the server) | Full (H) | access, account/folder, GTK approval |
| `list_drafts` | `account_id` | unsent draft metadata list | Read | access, account |
| `get_draft` | `account_id`, `draft_id` | one draft metadata object | Read | access, account/draft |
| `create_draft` | `account_id`; `to?`, `cc?`, `bcc?`, `subject?`, `body?`, `thread_id?`, `in_reply_to?`, `from?` (accepted but ignored) | saves draft; returns draft metadata | Draft (D) | access, account/input |
| `update_draft` | `account_id`, `draft_id`; `to?`, `cc?`, `bcc?`, `subject?`, `body?`, `thread_id?`, `in_reply_to?`, `from?` (accepted but ignored) | saves revision; returns draft metadata | Draft (D) | access, account/draft/input |
| `delete_draft` | `account_id`, `draft_id` | `deleted` boolean | Draft (D) | access, account |
| `reply_to_thread` | `account_id`, `thread_id`; `reply_all?` boolean | creates reply draft metadata | Draft (D) | access, account/thread |
| `forward_message` | `account_id`, `message_id` | creates forward draft metadata | Draft (D) | access, account/message |
| `send_draft` | `account_id`, `draft_id` | queues one SMTP operation after approval | Send (H) | access, account/draft, GTK approval |
| `attach_file_to_draft` | `account_id`, `draft_id`, `path` | draft-attachment metadata | Draft (D) | access, account/draft, safe root |
| `remove_attachment_from_draft` | `account_id`, `draft_id`, `attachment_id` | `removed` boolean | Draft (D) | access, account |
| `list_attachments` | `account_id`, `message_id` | incoming attachment metadata list | Read | access, account/message |
| `get_attachment` | `account_id`, `attachment_id` | one incoming attachment metadata object | Read | access, account/attachment |
| `download_attachment` | `account_id`, `attachment_id` | streams cached file to safe root; metadata | Read | access, account/attachment, safe root/cache |
| `save_attachment` | `account_id`, `attachment_id`, absolute `path` | streams cached file to safe-root path; metadata | Read | access, account/attachment, safe root/cache |
| `list_draft_attachments` | `account_id`, `draft_id` | draft-attachment metadata list | Read | access, account/draft |

`sync_account` does not itself talk to IMAP. The stdio server is a separate
process from the Feather Mail window and holds no handle on the sync worker,
so the tool records one durable pending sync per account; the running window
claims it within about half a second and wakes the worker, which then services
every configured account the same way the Diagnostics "Sync now" button does.
Repeated calls collapse onto that one pending request. With no window running,
the request waits and is honoured when one next opens — `queued: true` means
recorded, never "already fetched".

`access` always includes the live MCP switch, enrolled enabled profile,
persisted level, process level and account ceilings. It fails closed as
`PERMISSION_DENIED`. Missing/wrong required values, malformed cursors,
unknown tool names and impossible local folder choices are
`INVALID_ARGUMENT`; Core identity lookup uses `ACCOUNT_NOT_FOUND` or
`MESSAGE_NOT_FOUND` as applicable. A cache miss is
`OPERATION_NOT_SUPPORTED`; a draft attachment exceeding the Core limit is
`ATTACHMENT_TOO_LARGE`. High-risk approval is only GTK: before Allow once or
Always allow, after denial/expiry/staleness, or when any `confirm` field is
supplied to a high-risk tool, no mutation is queued.

## Error contract

For a syntactically valid `tools/call`, a domain failure is a normal MCP result
with `isError: true`, a human-readable `content` item, and a machine-readable
`structuredContent.code`. Callers must branch on that code, not the message.
Possible Core codes are `ACCOUNT_NOT_FOUND`, `MESSAGE_NOT_FOUND`,
`PERMISSION_DENIED`, `NETWORK_UNAVAILABLE`, `AUTH_REQUIRED`, `CONFLICT`,
`INVALID_ARGUMENT`, `OPERATION_NOT_SUPPORTED`, and
`ATTACHMENT_TOO_LARGE`.

Malformed JSON-RPC method parameters, such as a missing `tools/call` name,
return JSON-RPC `-32602` rather than a tool result. Resource authorization and
lookup errors use JSON-RPC `-32000` with the Core code in `data.code`; see
[resources](resources.md).

Thread reads include sender and subject but never preview or body text.
`list_thread_messages` accepts `account_id` and `thread_id` and returns the
metadata for that thread's messages without the enclosing thread projection.
`get_message` accepts one account-scoped `message_id` and returns only id,
thread/folder identity, date, sender, subject and unread/starred/attachment
state; it never returns account duplication, provider UID, raw headers, size,
cache data, preview or body.
Draft reads and draft-creation results likewise contain only identity,
threading and header metadata; `body` is accepted only as create/update input
and is never returned. `search_mail` uses the exact parser used by the GTK
search field. Mutations report the durable operation ids they created.

`list_threads`, `list_snoozed`, and `search_mail` use the same typed
continuation cursor:
omit `after` for the first page, then pass a non-null returned
`next: {"date": integer, "id": string}` unchanged as the next call's
`after`. `next: null` means there is no later page; the value is structured,
not an opaque token.

`list_snoozed` is read-only. It uses the same local Snoozed virtual-folder
query as the GTK sidebar, so it returns only threads whose local snooze has
not yet been woken by Core's scheduler; it neither contacts a provider nor
changes the snooze deadline.

`get_folder_message_count` accepts an `account_id` and a `folder_id` returned
by `list_folders`. It returns only that folder's current `unread` and `total`
from the same Core `FolderSummary` used by GTK, including virtual folders such
as Snoozed and Starred. It neither synchronizes nor contacts a provider.

`get_account_status` accepts one `account_id` and returns only its persisted
Core `status` (`synced`, `syncing`, `offline`, or `error`), without account
name or email. It reports no invented live-connection state and neither
synchronizes nor contacts a provider; an unknown account returns
`ACCOUNT_NOT_FOUND`.

`bulk_archive`, `bulk_mark_read`, `bulk_mark_unread`, `bulk_star`, and
`bulk_unstar` accept
1…100 unique non-empty thread ids and invoke their matching vector-valued Core
command / `dispatch_with_receipt` door. Archive changes the local Archive
placement, not an arbitrary destination folder; `bulk_star` and `bulk_unstar`
are set-only, not batch toggles, so every selected thread ends starred or
unstarred respectively. The MCP cap bounds one stdio request to at most 100
durable operations in one Core transaction. A malformed list is rejected before
dispatch; an unknown thread returns generic `MESSAGE_NOT_FOUND` without
identifying it, and the transaction leaves every requested placement/flag and
queue row unchanged. The success result contains only `queued` and opaque
operation ids.

`move_message` accepts an `account_id`, `thread_id`, and a destination
`folder_id` for an existing **custom** folder returned by `list_folders`. It
calls the same Core `Command::Move` door as GTK, so the local move and durable
operation share its queue/undo semantics. System and virtual folder ids, an
unlisted destination, or a folder belonging to another account return
`INVALID_ARGUMENT`; an unknown thread returns `MESSAGE_NOT_FOUND`; an account
outside the access ceiling returns `PERMISSION_DENIED`. This intentionally
does not invent a generic system-folder move contract: the current public Core
summary does not distinguish a real provider folder from its stable
placeholder.

`bulk_move` applies that exact custom-destination policy to 1…100 unique
non-empty thread ids in one existing vector `Command::Move` /
`dispatch_with_receipt` transaction. It returns only `queued` and opaque
operation ids. Malformed ids and invalid system, virtual, unlisted, or foreign
destinations fail before dispatch; an unknown thread is a generic
`MESSAGE_NOT_FOUND` atomic no-op without disclosing its id.

`bulk_snooze` accepts 1…100 unique non-empty thread ids and the same integer
Unix UTC `until` as `snooze_message`. It invokes the existing vector
`Command::Snooze` / `dispatch_with_receipt` door used by GTK selection, setting
one shared local deadline and durable local undo receipt per selected thread;
it does not queue IMAP/provider work. A malformed deadline or batch fails
before dispatch, and an unknown thread is a generic `MESSAGE_NOT_FOUND` atomic
no-op without disclosing its id.

`unsnooze_message` brings one snoozed thread back to Inbox now. It runs the
*same* local transition the snooze timer would have run at the deadline --
literally the same statements as `Core::wake_due_snoozes`, with the deadline
test dropped -- so the thread returns to Inbox, the local snooze ledger row is
cancelled, and the snooze is gone. It is not Undo: Undo restores whatever the
thread looked like before it was snoozed, which is a different end state. Like
every snooze operation it is a local overlay (D26), so `queued` is always
`false` and nothing is owed to IMAP. `unsnoozed: false` means the thread was
not snoozed -- the end state asked for already held -- and is deliberately not
an error; an unknown thread in a known account reports the same.

`rename_folder` renames one custom folder. The caller supplies only the new
leaf name; Core computes the destination mailbox path from the folder's stored
`remote_id` and the server-reported hierarchy delimiter, so a nested
`Team/Ideas` renamed to "Plans" becomes `Team/Plans` and is never promoted to
the top level. The local display name changes immediately and the folder's
identity (`remote_id`) moves only when the server acks the `RENAME`, in the
same transaction that acks the operation -- so a crash or a terminal failure
in between leaves the folder findable under the name it still has on the
server, and the next `LIST` walk puts the label back. Refusals come from Core
verbatim: an empty name, a system folder's name, a sibling name already taken,
a system folder as the target, or a folder that has not yet reached the server
(it has no mailbox to rename). Renaming to the name the folder already has is
not an error; it returns `queued: false` and puts nothing on the wire, because
`RENAME x x` is a server error rather than a no-op.

`delete_folder` deletes one custom folder, and is the only folder tool at
Full/high risk: a mailbox deletion is irreversible on the server and outside
the Undo history. Core, not MCP, computes the approval fingerprint -- a
SHA-256 over its own domain separator, the account and the folder id -- so an
Allow once for one folder can never be spent on another, and the dialog shows
a count of one rather than any id. The approval is consumed in the same SQLite
transaction as the deletion, so an approval is never burned without an effect
and an effect never happens on a spent approval.

Core applies the same two refusals it applies to a user: the folder must be a
custom folder, and it must be empty. A folder that still holds mail is refused
outright rather than having its mail deleted or silently moved -- and because
the refusal aborts the transaction, an approval spent on a folder that filled
up in the meantime survives for a retry after the mail is moved. On the wire
the applier re-checks emptiness with a `SELECT` immediately before `DELETE`,
since mail can arrive between the local check and the queue draining.

Locally the folder row survives as a tombstone (`deleted_at`): `messages` and
the durable Undo history reference folders by id, so removing the row would
take real history with it. It disappears from `list_folders` at once, but
keeps its `remote_id` until the server acks the `DELETE`, so a `LIST` walk in
between adopts this row rather than creating a second folder for the mailbox
that still exists -- and a terminal failure self-heals: the next walk finds
the mailbox and brings the folder back with its mail. Deleting an
already-deleted folder is not an error; it reports `queued: false`. So does
deleting a folder that had never reached the server, whose pending `CREATE` is
cancelled instead. Creating a folder with the same name afterwards revives the
tombstone rather than reporting a duplicate the caller cannot see.

`bulk_delete` accepts 1…100 unique non-empty thread ids and moves those
conversations to Trash through the same vector `Command::Trash` path as GTK.
It is Full/high-risk: Core, not MCP, canonicalizes the unordered id set and
stores only a SHA-256 batch digest plus its count. Thus an Allow once approval
matches exactly that set regardless of input order; adding or removing an id
needs a fresh GTK decision. Core consumes a matching Allow once and performs
the entire vector dispatch in one SQLite transaction, so a failed/unknown
target leaves both the approval and every local placement/queue row unchanged.
The dialog shows only the number of conversations, never ids, account data,
subjects, bodies, arguments, or the digest. `Always allow` remains a per-tool
grant and still obeys the live switch plus Full/account ceilings.

`bulk_permanent_delete` takes the same bounded id list but invokes the distinct
vector `Command::PermanentDelete` path already used by GTK's multi-selection.
It bypasses Trash and creates no undo ticket. Core binds its own length-framed,
order-independent SHA-256 digest and safe count to this capability — it is not
interchangeable with `bulk_delete` — then consumes matching Allow once and
dispatches the entire irreversible vector in one immediate SQLite transaction.
The GTK dialog shows only the number of conversations. Reordered ids reuse an
approval; a changed/unknown batch, expiry, revoke, disabled MCP, or an
insufficient/foreign ceiling fails closed without a partial queue/mailbox
mutation or raw id in an error or audit row.

`permanent_delete` accepts one `account_id` and `thread_id`, then invokes the
same irreversible `Command::PermanentDelete` path as GTK. It is a Full
high-risk action: the MCP call cannot confirm itself and must receive the
separate GTK **Deny / Allow once / Always allow** decision. Before approval,
on an expired/denied request, or outside the account ceiling it returns
`PERMISSION_DENIED` without queueing a mutation. It bypasses Trash and has no
undo ticket.

`restore_message` does not accept an operation id or act as generic
`undo(ticket)`. Core first requires the requested thread to be currently
trashed (overlay or discovered real Trash folder), finds only its exact
reversible pending/running/acked Trash intent, and refuses it if a newer
placement-changing intent exists. A pending Trash is
cancelled and the exact local snapshot is restored, returning
`{restored:true, queued:false}`. If Trash may already have reached the
provider, Core creates the existing causal reverse Move and returns only its
opaque id as `{restored:true, queued:true, operation_id}`. An unknown thread is
`MESSAGE_NOT_FOUND`; a known non-trashed thread, missing reversible lifecycle,
or permanent deletion is `OPERATION_NOT_SUPPORTED`. It never guesses Inbox as
a destination or exposes original operation/ticket ids.

`reply_to_thread` creates Reply by default; `reply_all: true` includes the
original To/Cc recipients that are not the current account or primary sender.
`forward_message` creates a recipient-free draft for a specific message.
Both create editable local drafts only; `send_draft` remains the separate
confirmed high-risk operation.

Incoming-attachment reads return only id, message id, filename, MIME type,
size and local-cache state — never bytes or the private cache path.
`download_attachment` chooses a sanitized new filename below
`FEATHERMAIL_MCP_ATTACHMENT_ROOT`; `save_attachment` takes an absolute new
path below that same root. Both stream an already cached file and refuse to
overwrite a destination. MCP never opens an IMAP connection; a cache miss is
reported honestly so it can be downloaded through the running client first.
