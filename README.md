<p align="center">
  <img src="site/assets/mark.png" width="72" height="72" alt="Feather Mail">
</p>

# Feather Mail

A fast, minimal email client built natively for Linux with Rust, GTK4 and Relm4.

<p>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-1a64fc?style=flat-square" alt="GPL-3.0-or-later"></a>
  <a href="https://github.com/prodocik/feathermail/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/prodocik/feathermail/ci.yml?style=flat-square&label=CI" alt="CI"></a>
  <a href="https://github.com/prodocik/feathermail/releases/latest"><img src="https://img.shields.io/github/v/release/prodocik/feathermail?style=flat-square&label=release" alt="Latest release"></a>
  <a href="https://github.com/sponsors/prodocik"><img src="https://img.shields.io/github/sponsors/prodocik?style=flat-square&label=sponsors" alt="GitHub Sponsors"></a>
</p>

[Website](https://prodocik.github.io/feathermail/) · [Download v0.1](https://github.com/prodocik/feathermail/releases/latest) · [Install guide](docs/install.md) · [MCP guide](docs/mcp/overview.md) · [Changelog](CHANGELOG.md)

![Feather Mail showing a local-first three-pane inbox](site/assets/inbox.png)

## Fast by default, minimal by design

- The inbox opens from local cache while synchronization continues in the background.
- A calm three-pane interface keeps controls close and clutter out.
- Local FTS5 search answers without a network round-trip on every keystroke.
- Several accounts work in one window, including a unified All Accounts view.
- IMAP/SMTP synchronization runs behind a durable offline command queue.
- HTML mail renders with JavaScript disabled, a restrictive CSP and remote images blocked by default.
- Search, compose, archive, snooze, reply and navigation are keyboard-friendly.
- Optional MCP over stdio uses the same Core command bus as the GTK interface and is off by default.
- No telemetry, ads, paid cloud mailbox or “Pro” feature lock.

## Install

The [v0.1 release](https://github.com/prodocik/feathermail/releases/latest) provides x86_64 `.deb` and AppImage packages. Both require GTK 4.10 or newer and the WebKitGTK 6.0 runtime; the AppImage intentionally uses the host WebKitGTK so its helper processes stay version-matched.

```bash
# Ubuntu / Debian / Linux Mint
sudo apt install ./feathermail_0.1.1_amd64.deb

# Other recent x86_64 distributions
chmod +x Feather-Mail-0.1.1-x86_64.AppImage
./Feather-Mail-0.1.1-x86_64.AppImage
```

See [docs/install.md](docs/install.md) for dependencies, source builds and provider notes.

## Account support in v0.1

The Add Account screen configures manual IMAP/SMTP. Google and Yandex users should use provider app passwords. Microsoft 365 and Outlook.com commonly require Modern Auth and may reject password-only clients; interactive Microsoft OAuth is not exposed in this release. The UI says this before credentials are submitted.

Secrets are stored in the desktop Secret Service keyring, never in SQLite. If no keyring is available, Feather Mail refuses to save the account password.

## Build from source

Rust 1.93+, GTK 4.10+ and WebKitGTK 6.0 development files are required.

```bash
sudo apt install git libgtk-4-dev libwebkitgtk-6.0-dev pkg-config
git clone https://github.com/prodocik/feathermail.git
cd feathermail
cargo run -p feathermail
```

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Project policy

Feather Mail is [GPL-3.0-or-later](LICENSE) free software and is maintained by [prodocik](https://github.com/prodocik). Forks are welcome under the GPL. This canonical repository is solo-maintained and does not merge outside commits; bug reports and private [security reports](SECURITY.md) are welcome.

Development is supported by donations, not by selling inbox data: [GitHub Sponsors](https://github.com/sponsors/prodocik).
