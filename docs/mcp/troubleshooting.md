# MCP troubleshooting

- `PERMISSION_DENIED`: enable MCP in Feather Mail, use the enrolled `stdio`
  identity (unknown identities are denied), ensure the process ceiling and
  account allowlist do not narrow access, then answer the GTK dialog for Send
  or Delete. A revoked profile remains revoked after off/on or restart; with
  global MCP on, use Settings → AI & MCP → **Re-enable access** and confirm to
  start a fresh Read + Draft profile. A `confirm` field cannot bypass high-risk
  approval and is `INVALID_ARGUMENT` on those actions.
- `INVALID_ARGUMENT`: inspect the `tools/list` input schema. Missing or empty
  required strings, an unknown tool, malformed cursor, or an impossible local
  folder/destination are rejected before a mutation is queued.
- `ACCOUNT_NOT_FOUND` or `MESSAGE_NOT_FOUND`: refresh local metadata and use
  an id from an allowed account. A draft or attachment lookup uses the same
  local target boundary as a message lookup and does not reveal a foreign id.
- `OPERATION_NOT_SUPPORTED`: an incoming attachment is not available in the
  local cache yet, or its local data cannot be exported through the safe root.
  Download it through the running client first; MCP never starts IMAP sync.
- Attachment denied: set `FEATHERMAIL_MCP_ATTACHMENT_ROOT` and keep the file
  below that canonical directory.
- Tool mutation is queued but not on the server: the app may be offline;
  inspect Settings → Diagnostics and the durable pending/failed counts.
- Settings changed while connected: no stdio restart is needed. Every
  tool/resource action reads the live in-app switch; turning it off takes
  effect on the next action.

For a valid `tools/call`, a Core/domain failure is represented as a normal
MCP result with `isError: true`, a human message, and
`structuredContent.code`. Use that code rather than matching the message.
Malformed JSON-RPC method parameters are `-32602`; authorization and lookup
failures from `resources/list` or `resources/read` are JSON-RPC `-32000` with
the Core code in `data.code`.
