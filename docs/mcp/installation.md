# MCP installation

Enable **Settings → AI & MCP → Local MCP server**, then configure the
client to launch `feathermail-mcp` over stdin/stdout.

Optional process settings:

- `FEATHERMAIL_MCP_CLIENT_ID`: enrolled profile identity (default `stdio`).
  An unknown id is denied; an environment variable never creates a profile.
- `FEATHERMAIL_MCP_PERMISSION`: `read`, `draft`, `send`, or `full` ceiling.
  It only narrows persisted Core policy and cannot grant Send/Delete.
- `FEATHERMAIL_MCP_ACCOUNTS`: comma-separated account ceiling.
- `FEATHERMAIL_MCP_ATTACHMENT_ROOT`: the only directory attachments may be
  read from; attachment access is denied when it is absent.

The in-app switch is checked for each action; a running stdio process needs no
restart after it changes. Enabling provisions a fresh local `stdio` row at
Read + Draft only when it is absent. That global enable never re-enables a revoked `stdio`
row, including after an off/on cycle or restart. With the global switch on,
Settings exposes a separately confirmed **Re-enable access** control only for
that existing revoked local row; it starts a fresh Read + Draft/no-grants
policy rather than restoring earlier permissions. Send/Delete wait for a GTK
**Deny / Allow once / Always allow** decision; clients must not supply
`confirm`.
