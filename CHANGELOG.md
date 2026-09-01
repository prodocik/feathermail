# Changelog

## 0.1.0 — 2026-09-01

First public Feather Mail release.

### Highlights

- Native GTK4/Relm4 three-pane mail client for Linux with multiple accounts and All Accounts views.
- Local SQLite cache, background IMAP/SMTP synchronization and a durable offline operation queue.
- Threaded reading, compose/reply/forward, drafts, attachments, archive, trash, star, mark read, move, snooze and undo.
- Local FTS5 search with operators and globally ordered results across accounts.
- Safe HTML rendering with JavaScript disabled, CSP isolation, tracking protection, remote-image controls and a bounded CSS allowlist that preserves common sender layouts.
- Inline CID raster images and system-default opening for downloaded attachments.
- Recipient autocomplete from received and successfully sent mail.
- Keyboard navigation, desktop notifications, update checks and pull/wheel-to-refresh.
- Optional local MCP stdio server, disabled by default, sharing the same Core command bus and permission model as the UI.
- `.deb` and AppImage packages for x86_64 Linux.

### Privacy

- No telemetry.
- Passwords and tokens use the system keyring, never SQLite or diagnostic exports.
- Remote images and tracking pixels are blocked by default; link confirmation and plain-text preference are functional settings.

### Known limitations

- Add Account is manual IMAP/SMTP in 0.1. Google and Yandex generally require app passwords. Microsoft 365/Outlook.com may reject password-only authentication; interactive Microsoft OAuth is not yet available in the UI.
- The runtime floor is GTK 4.10 and WebKitGTK 6.0. The AppImage intentionally relies on the host WebKitGTK runtime.
- Packages were built and smoke-tested on Ubuntu 25.10. Other distributions listed in the install guide match the required package versions but have not all been exercised yet.
