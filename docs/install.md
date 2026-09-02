# Install Feather Mail

Feather Mail is a native x86_64 Linux application. Windows, macOS, Flatpak, Snap, rpm and ARM packages are not provided yet. The current version and its changes are listed on the [releases page](https://github.com/prodocik/feathermail/releases/latest).

## Requirements

- GTK 4.10 or newer
- WebKitGTK 6.0 (`libwebkitgtk-6.0.so.4`)
- A desktop Secret Service such as GNOME Keyring or KWallet for account passwords

The packages were built and smoke-tested on Ubuntu 25.10. Ubuntu 24.04+, Debian 13, Linux Mint 22+, Fedora 40+ and current Arch provide the required GTK/WebKit generation; those distributions have not all been exercised by the project yet. Ubuntu 22.04 and Debian 12 are below the GTK/WebKit floor.

## Debian, Ubuntu and Linux Mint

Download the `.deb` package from [GitHub Releases](https://github.com/prodocik/feathermail/releases/latest), then install it with dependency resolution:

```bash
sudo apt install ./feathermail_*_amd64.deb
```

Launch **Feather Mail** from the app grid or run `feathermail`.

## AppImage

Download the AppImage from [GitHub Releases](https://github.com/prodocik/feathermail/releases/latest):

```bash
chmod +x Feather-Mail-*-x86_64.AppImage
./Feather-Mail-*-x86_64.AppImage
```

The AppImage deliberately does not bundle WebKitGTK: WebKit spawns version-matched helper processes from the host. Install your distribution's WebKitGTK 6.0 runtime if the library is missing.

## Build from source

Install Rust 1.93 or newer and the GTK/WebKit development packages.

```bash
# Ubuntu / Debian
sudo apt install git libgtk-4-dev libwebkitgtk-6.0-dev pkg-config

# Fedora
sudo dnf install git gtk4-devel webkitgtk6-devel pkgconf-pkg-config

# Arch
sudo pacman -S git gtk4 webkitgtk-6.0 pkgconf
```

```bash
git clone https://github.com/prodocik/feathermail.git
cd feathermail
cargo run -p feathermail
```

## Add an account

Add Account asks first where the mailbox lives: **IMAP** or **Linux**.

**Linux** lists the accounts this desktop is already signed into (GNOME Online Accounts) that carry mail; one press adds the mailbox, with no browser step and no password. The access token comes from the session's own account manager over D-Bus, so Feather Mail carries no Google client ID or secret for this path. Revoke access in Settings -> Online Accounts. If the session has no such manager, that level says so and you use the IMAP branch.

**IMAP** configures IMAP/SMTP with presets for Google, Microsoft and Yandex plus a form for your own server. After you enter your address it looks up the provider's servers (Thunderbird ISPDB and DNS SRV) and fills in the fields you left empty; every field stays editable. Google and Yandex users should create a provider app password. Microsoft 365 and Outlook.com commonly require Modern Auth and may reject a password-only client; interactive Microsoft OAuth is not available yet.

Feather Mail stores mail data under `~/.local/share/feathermail/`, caches bodies and attachments under `~/.cache/feathermail/`, and keeps credentials only in the session Secret Service keyring.

## Updates

Settings → About → **Check for updates** reads the latest release from this GitHub repository. It reports a newer version and opens the release page; it never installs an update silently.
