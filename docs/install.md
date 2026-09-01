# Install Feather Mail

Feather Mail 0.1 is a native x86_64 Linux application. Windows, macOS, Flatpak, Snap, rpm and ARM packages are not included in this release.

## Requirements

- GTK 4.10 or newer
- WebKitGTK 6.0 (`libwebkitgtk-6.0.so.4`)
- A desktop Secret Service such as GNOME Keyring or KWallet for account passwords

The packages were built and smoke-tested on Ubuntu 25.10. Ubuntu 24.04+, Debian 13, Linux Mint 22+, Fedora 40+ and current Arch provide the required GTK/WebKit generation; those distributions have not all been exercised by the project yet. Ubuntu 22.04 and Debian 12 are below the GTK/WebKit floor.

## Debian, Ubuntu and Linux Mint

Download `feathermail_0.1.1_amd64.deb` from [GitHub Releases](https://github.com/prodocik/feathermail/releases/latest), then install it with dependency resolution:

```bash
sudo apt install ./feathermail_0.1.1_amd64.deb
```

Launch **Feather Mail** from the app grid or run `feathermail`.

## AppImage

Download `Feather-Mail-0.1.1-x86_64.AppImage` from [GitHub Releases](https://github.com/prodocik/feathermail/releases/latest):

```bash
chmod +x Feather-Mail-0.1.1-x86_64.AppImage
./Feather-Mail-0.1.1-x86_64.AppImage
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

Version 0.1 exposes manual IMAP/SMTP configuration. Google and Yandex users should create a provider app password. Microsoft 365 and Outlook.com commonly require Modern Auth and may reject a password-only client; interactive Microsoft OAuth is not available in this UI release.

Feather Mail stores mail data under `~/.local/share/feathermail/`, caches bodies and attachments under `~/.cache/feathermail/`, and keeps credentials only in the session Secret Service keyring.

## Updates

Settings → About → **Check for updates** reads the latest release from this GitHub repository. It reports a newer version and opens the release page; it never installs an update silently.
