# LocalBeam

**Fast, private, local-network file sharing — no internet, no accounts, no cloud.**

LocalBeam lets you beam files between devices on the same Wi-Fi or LAN instantly. It lives in your system tray, discovers peers automatically via mDNS (like AirDrop), and transfers files over a direct TLS connection — nothing leaves your network.

![LocalBeam screenshot](docs/screenshot.png)

---

## Features

- **Zero-config discovery** — peers appear automatically via mDNS/Bonjour (`_localbeam._tcp.local.`)
- **Direct TLS transfer** — encrypted peer-to-peer over TCP; no relay server
- **AirDrop-style UI** — radar animation, flat device icons, per-transfer accept/reject prompts
- **Contacts system** — allow-all mode or contacts-only mode with contact requests
- **Mobile uploads via QR** — scan the QR code in the app to upload files from your phone's browser
- **System tray** — lives quietly in the menu bar (macOS) or taskbar (Windows/Linux); no dock icon on macOS
- **Auto-save to Documents/LocalBeam** — received files land in `~/Documents/LocalBeam` by default
- **Open folder on complete** — click the folder icon next to a finished transfer to reveal it in Finder/Explorer

---

## Installation

Download the latest release for your platform from the [Releases](../../releases) page:

| Platform | File |
|---|---|
| macOS (Apple Silicon) | `.dmg` (aarch64) |
| macOS (Intel) | `.dmg` (x86_64) |
| Windows | `.msi` or `.exe` |
| Linux | `.AppImage` or `.deb` |

---

## Building from source

### Prerequisites

- [Rust](https://rustup.rs/) (stable)
- [Node.js](https://nodejs.org/) LTS
- Tauri CLI v2 — installed automatically via `npm install`

**Linux only:**
```bash
sudo apt-get install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libasound2-dev pkg-config
```

### Development

```bash
npm install
npm run tauri dev
```

### Production build

```bash
npm run tauri build
```

Installers are written to `src-tauri/target/release/bundle/`.

---

## How it works

1. **Identity** — on first launch, LocalBeam generates a self-signed TLS certificate. The certificate's SHA-256 fingerprint is your device ID. It persists in your app data directory.
2. **Discovery** — LocalBeam advertises itself on the local network via mDNS and listens for other LocalBeam instances. No router configuration required.
3. **Transfer** — when you send files, LocalBeam opens a direct TLS connection to the peer. The receiver sees an accept/reject prompt. Accepted files are streamed directly to disk with live progress.
4. **Mobile uploads** — an embedded HTTP server (disabled by default, opt-in in settings) serves a small upload page. Scan the QR code in the app to reach it from any browser on the same network.

---

## Settings

| Setting | Default | Description |
|---|---|---|
| Display name | hostname | Name shown to peers |
| Save directory | `~/Documents/LocalBeam` | Where received files are saved |
| Receive mode | Allow all | `allow_all` or `contacts_only` |
| QR upload server | Disabled | Enables mobile browser uploads |
| QR server port | 7878 | Port for the mobile upload server |

---

## Privacy & security

- All peer-to-peer transfers use TLS 1.3 with mutual certificate verification.
- Device identity is a self-signed certificate stored locally — no CA or account required.
- No telemetry, no analytics, no internet connections.
- The optional mobile upload server is HTTP only and binds to `0.0.0.0` on your local network — enable it only on trusted networks.

---

## Contributing

Pull requests are welcome. Please open an issue first for significant changes.

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/my-feature`)
3. Commit your changes
4. Push and open a pull request

---

## License

[MIT](LICENSE) © LocalBeam Contributors
