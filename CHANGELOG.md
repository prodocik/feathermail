# Changelog

## 0.3.0 — 2026-09-02

Adding a mailbox is the whole of this release: accounts you are already signed into on this desktop can be
added with one click, and the Add-account screen was rebuilt around that choice.

### Add account

- **Sign in with a desktop account.** Feather Mail reads the accounts your desktop already holds (GNOME
  Online Accounts) and lists the ones that carry mail. One click adds the mailbox: the token comes from the
  session account manager, so there is no browser window, no password, and no client secret in the app.
- **The Add-account screen now has three levels.** The first asks where the mailbox lives — *IMAP* or
  *Linux*. *IMAP* leads to the familiar presets (Google, Microsoft, Yandex) and the manual form; *Linux*
  lists the accounts you signed into on this desktop. If there are none, that level explains how to add one
  in Settings instead of showing an empty list. **Back** returns to the level you came from.
- Both first-level buttons carry an icon, and their label and hint line up on a single left edge.

### Reading mail

- The message header now names the address the letter actually arrived at instead of saying "to me" — the
  one that matters when All accounts mixes several mailboxes into one list. When a message went to several
  recipients, the header names your own mailbox among them.

### Fixes

- **XOAUTH2 sign-in to Gmail failed with "Couldn't reach the server."** The IMAP reply to `AUTHENTICATE` was
  read one line deep, and Gmail answers a successful bind with an untagged `* CAPABILITY` line *before* the
  tagged `OK`. That first line was mistaken for the result. Both the probe made while adding a mailbox and
  every live session shared the bug; they now share one corrected implementation that reads on to the tagged
  line.

## 0.2.0 — 2026-09-02

A stability release driven by a full audit of the client. No new account types; the fixes below cover
reading, syncing, searching, composing and the MCP server.

### Reading mail

- A `<style>` element with any attribute no longer bypasses the CSS allowlist, and `>` / `&` inside author
  CSS no longer break the stylesheet.
- MIME parts whose boundary line carries trailing whitespace (RFC 2046 transport padding) now render instead
  of showing an empty body.
- Attachment names encoded per RFC 2231 (`filename*=`) are decoded, so they keep their real file names.
- Replying to an HTML-only message now quotes its text.
- The folder chip next to an open message names the real folder instead of always saying "Inbox"; list rows
  in Starred, Snoozed and All accounts now carry a folder chip too.
- Times in the list and reading pane are computed from the real clock. Previously every message newer than
  May 2024 showed a time of day instead of a date.
- Folder names are decoded consistently in the sidebar, the list header and the message view; sender names
  in thread cards are RFC 2047-decoded.
- Secondary and tertiary text colours were darkened in both themes to clear WCAG AA contrast.

### Sync and folders

- Mail deleted on the server is now removed locally. On CONDSTORE servers a sliding walk reconciles the whole
  mailbox, one 200-UID batch per pass, and resumes after a restart.
- CONDSTORE is actually enabled on SELECT, so flag changes from other clients arrive incrementally.
- A date header with a non-ASCII month name no longer panics the sync thread.
- Deleting an account no longer leaves a stale backoff record that spun the sync worker.
- A cancelled sync pass is no longer recorded as a completed one.
- Creating and renaming folders with non-ASCII names sends modified UTF-7 to the server, and renaming under
  a non-ASCII parent no longer double-encodes the prefix.
- Removing vanished messages also removes their cached bodies from disk, and "Clear cache" really deletes
  the files.
- The OAuth reauthorisation flow keeps the refreshed refresh token, and a `%` before a multi-byte character
  in a redirect no longer panics.

### Queue and drafts

- Repeating a command after an identical one was acknowledged is queued again instead of being silently
  dropped.
- Queued operations replay in the order you issued them, even when one was retried later.
- Permanent delete acts on the messages that were in the thread when you asked, not whatever arrived since.
- Deleting a thread that a reply draft points to no longer fails with a foreign-key error and stalls sync.
- Drafts: a failed sequence lookup no longer overwrites an existing draft; discarding a draft while
  autosave is in flight no longer resurrects it; edits made just before closing the compose window are kept.
- Reply and forward from All accounts save and send through the draft's own account.
- Opening a search hit from All accounts switches to the hit's account instead of an empty list.
- Star, search history, folder create/rename/delete, account rename/removal and move all run off the UI
  thread; the interface never blocks on SQLite.

### Search

- Words inside Japanese and Chinese text are found (e.g. "東京" inside a sentence), and "ё" matches "е"
  in both directions. Existing profiles reindex in the background after the upgrade.

### Notifications

- A burst of more than 20 new messages is announced as one summary notification instead of losing
  everything below the cut.
- A mailbox added in the current session starts its notification watermark at its newest message, so the
  first sync does not announce the whole history.

### Add account

- The "Other IMAP" form looks up IMAP/SMTP settings (Thunderbird ISPDB and DNS SRV) when you leave the
  email field, filling only the fields you have not typed into.

### MCP server

- `send_email` no longer bumps the draft revision on every call for bodies with surrounding whitespace.
- `create_draft` / `update_draft` no longer accept an arbitrary `from` address; `update_draft` without a
  `draft_id` is rejected instead of creating a new draft.
- Waiting for a UI confirmation no longer floods the audit log.
- The permissions and tools documentation matches the registered tool set and the real `limit` behaviour.

### Storage

- The database uses `IMMEDIATE` transactions for read-then-write paths, so a concurrent writer waits instead
  of failing with `SQLITE_BUSY_SNAPSHOT`.
- Cached bodies and attachments are written with owner-only permissions.
- Schema upgraded to v29.

## 0.1.1 — 2026-09-01

- Pull-to-refresh no longer animates the bubble's layout margin, avoiding repeated layout work while the gesture follows the pointer or wheel.
- The lightweight opacity and burst feedback remain unchanged, including reduced-motion behavior.
- A regression test now rejects transitions of layout properties in the shell CSS.

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
