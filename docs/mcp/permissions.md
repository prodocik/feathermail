# MCP permissions

| Level | Capability ceiling |
|---|---|
| Read | R: accounts, folders, threads, search, draft/attachment metadata |
| Draft | Read + D/W/M: draft edits, folder creation and ordinary mailbox mutations |
| Send | Draft + may ask GTK for the H Send action; it is not an automatic grant |
| Full | Send + may ask GTK for H Delete actions; it is not an automatic grant |

The registered tools have this explicit capability-matrix mapping; the test
suite fails if a registered tool lacks one.

| Matrix | Current tools | D57 level |
|---|---|---|
| R | `list_accounts`, `get_account`, `get_account_status`, `list_folders`, `get_folder`, `get_folder_message_count`, `list_threads`, `list_snoozed`, `get_thread`, `list_thread_messages`, `get_message`, `get_messages`, `search_mail`, `list_drafts`, `get_draft`, `list_attachments`, `get_attachment`, `download_attachment`, `save_attachment`, `list_draft_attachments` | Read |
| D | `create_draft`, `update_draft`, `delete_draft`, `reply_to_thread`, `forward_message`, `attach_file_to_draft`, `remove_attachment_from_draft` | Draft |
| W | `archive_message`, `bulk_archive`, `mark_read`, `bulk_mark_read`, `mark_unread`, `bulk_mark_unread`, `star_message`, `unstar_message`, `bulk_star`, `bulk_unstar`, `sync_account` | Draft |
| M | `snooze_message`, `unsnooze_message`, `bulk_snooze`, `move_message`, `bulk_move`, `restore_message`, `create_folder`, `rename_folder` | Draft |
| H | `send_draft`, `send_email` | Send + GTK confirmation |
| H | `delete_message`, `bulk_delete`, `bulk_permanent_delete`, `permanent_delete`, `delete_folder` | Full + GTK confirmation |

After enabling MCP, Core provisions the known local `stdio` client at **Read
+ Draft**. Settings may change the persisted level only for that enabled
profile. Changing it is one Core transaction: the new level becomes the
ceiling for every action, all per-tool grants disappear, and pending or
unconsumed Allow-once rows become terminal `invalidated`. Thus an old Always
allow or a process `full` setting cannot raise Read/Draft. The operation never
enrols or re-enables a client. A process may set `read`, `draft`, `send`, or
`full`, but this is only another upper bound/intersection with the persisted
Core profile; a non-empty account allowlist is another upper bound.

The AI & MCP settings page lists bounded durable client status (profile name,
enabled/revoked state, and level). Its **Revoke access** control goes through
one Core transaction, which disables that profile, removes all of its per-tool
grants, and invalidates pending or unconsumed Allow-once approvals. A revoked
profile stays revoked across restart and a global MCP off/on cycle; only an
explicit Settings/Core operation can re-enable it. Only while
global MCP is on, the revoked local `stdio` row exposes **Re-enable access**:
a native Cancel-default confirmation calls one Core transaction that restores
only `enabled` + Draft and clears grants. It cannot create a row or alter a
different client, and historical terminal approval rows remain terminal. This
is a fresh policy, not restoration of a former Send/Full level or Always allow.

Send/Delete use GTK's **Deny / Allow once / Always allow** decision. Always is
per tool and still obeys the live on/off setting plus both ceilings. A denied,
expired, stale, or unanswered request returns `PERMISSION_DENIED`. For those
high-risk actions, any supplied `confirm` field (including `true`) is
`INVALID_ARGUMENT`; it is never an authority. There is no silent permission
upgrade and no `remove_account` tool.
