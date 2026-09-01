# MCP examples

List Inbox threads:

```json
{"name":"list_threads","arguments":{"account_id":"work","folder_id":"work:inbox","limit":50}}
```

For a later page, pass the prior response's non-null `next` object unchanged
as the optional `after` argument. It is always `{date, id}`; `next: null`
means the list is complete. `search_mail` uses the identical continuation
shape.

Create and send a draft:

```json
{"name":"create_draft","arguments":{"account_id":"work","from":"me@example.com","to":"you@example.com","subject":"Hello"}}
{"name":"send_draft","arguments":{"account_id":"work","draft_id":"draft:work:1"}}
```

The second call returns `PERMISSION_DENIED` while Feather Mail waits for the
user's GTK choice: **Deny**, **Allow once**, or **Always allow**. Retry after
Allow once or Always allow; never send `confirm: true` (it is rejected).
Sending only queues work. Delivery continues through the same offline worker
as the GUI.

Move a bounded batch to Trash:

```json
{"name":"bulk_delete","arguments":{"account_id":"work","thread_ids":["thread-1","thread-2"]}}
```

This also waits for the native GTK decision. The confirmation is bound to that
exact unordered set: retrying the same ids in another order is the same request;
adding or removing an id requires a new decision. The tool never accepts
`confirm: true` and returns only queued operation ids after approval.

Permanently delete a bounded batch:

```json
{"name":"bulk_permanent_delete","arguments":{"account_id":"work","thread_ids":["thread-1","thread-2"]}}
```

This uses a distinct confirmation from Trash, bypasses Trash, and has no undo
ticket. The same exact-set and `confirm: true` rules apply.
