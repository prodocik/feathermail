# Security

Feather Mail stores mail locally and talks to IMAP/SMTP. Treat secrets as secrets.

## Report a vulnerability

Please **do not** open a public issue for anything that can leak mail, tokens, or credentials.

1. Use [GitHub private vulnerability reporting](https://github.com/prodocik/feathermail/security/advisories/new) if it is enabled.
2. Otherwise email **prodocik@gmail.com** with steps to reproduce and impact.

We will acknowledge the report and work on a fix before any disclosure.

## Hard rules already in the project

- Passwords and OAuth tokens live in the system keyring, never in SQLite and never in git.
- Logs and diagnostic exports must not contain passwords, tokens, message bodies, or attachment bytes.
- MCP must not return credentials. High-risk tools (send, delete) require explicit permission.
- HTML mail is rendered with JavaScript off, in an isolated WebKit view.
