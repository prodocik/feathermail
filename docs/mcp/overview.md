# MCP in Feather Mail

`feathermail-mcp` is a local stdio MCP server over the same SQLite-backed
Core command bus as the GTK application. It never clicks the UI and never
imports GTK, IMAP, SMTP, SQLite, the keyring, or a shell.

The server is disabled by default. A fresh enable provisions the local `stdio`
profile at Read + Draft only when that row does not already exist; a revoked
profile stays revoked across a global off/on cycle or restart. Process
environment settings may narrow durable policy but never grant or enroll a
client. While the global switch is on, only an explicit Cancel-default Settings
confirmation may re-enable the existing revoked local `stdio` profile; it is
reset to Read + Draft with no grants. Read operations are local-first; mutations
enter the durable operation queue used by the GUI. Send/Delete require a GTK
Deny / Allow once / Always allow decision. Tool results are metadata-only: MCP
never returns a message preview or a message/draft body.
