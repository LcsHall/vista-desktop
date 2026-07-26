# Vista Platform — desktop app

Tauri 2 shell that hosts `https://app.vistainterface.com` inside a
borderless native window with a custom title bar, auto-updates, and
a path to the Microsoft Store.

## Architecture

One borderless window with two child webviews stacked vertically:

```
┌─────────────────────────────────────────┐
│ ▣ Vista Platform           — □  ✕       │  ← title bar (this repo)
├─────────────────────────────────────────┤
│                                         │
│   https://app.vistainterface.com        │  ← content webview
│   (the live Vercel-deployed app)        │
│                                         │
└─────────────────────────────────────────┘
```

- **Title bar** — HTML/CSS/JS in `/src`, served locally from the
  Tauri bundle. Drag region + Vista mark + window controls (min /
  max / close). Lives in this repo so the shell stays self-contained.

- **Content** — points at production. Every Vercel deploy of
  vista-platform updates the content for every installed desktop
  user, instantly. The only time we ship a new desktop binary is
  when something native changes (title bar, menus, tray, updater
  wiring, etc.).

This split is the whole point: **content updates without app
rebuilds**, **native chrome without coupling to web code**.

## What you need to install once (Windows)

1. **Rust** — `rustup-init.exe` from https://rustup.rs/. Tauri builds
   the native shell in Rust. Stable channel is fine. ~10 min install,
   ~1.5 GB on disk.
2. **WebView2 runtime** — already present on Windows 10 1809+ and all
   Windows 11. The Tauri installer falls back to bootstrapping it on
   older systems.
3. **Visual Studio Build Tools** with the **"Desktop development with
   C++"** workload — Tauri's Windows linker needs `link.exe`. Installer
   at https://visualstudio.microsoft.com/visual-cpp-build-tools/.
4. **Node.js 20+** — you already have this for vista-platform.

For Mac builds: Xcode (full IDE, not just CLI tools) once you want to
target macOS.

## Run it locally

```bash
npm install
npm run tauri dev
```

First launch downloads + compiles ~300 crates — budget ~5–10 min on
the first run, ~30 sec on subsequent runs (Cargo caches everything).

The window opens, loads the title bar, then loads
`app.vistainterface.com`. Sign in normally; auth + cookies persist
in WebView2's cookie store between launches.

## Generate app icons

Drop a 1024×1024 PNG of the Vista mark somewhere, then:

```bash
npm run tauri icon path/to/vista-source.png
```

This generates the full set under `src-tauri/icons/`. See
`src-tauri/icons/README.md` for source-image requirements.

## Auto-updater setup (one-time)

The updater plugin pings a `latest.json` manifest on launch + downloads
any newer signed build. Signing prevents an attacker who compromises
your CDN from pushing a malicious "update."

**1. Generate the signing keypair:**

```bash
npm run tauri signer generate -- -w ~/.tauri/vista.key
```

This emits two files:
- `~/.tauri/vista.key` — **private key**. Keeps releases signable.
  **NEVER commit this.** It belongs in the GitHub Actions secret
  `TAURI_SIGNING_PRIVATE_KEY` and on your offline backup.
- `~/.tauri/vista.key.pub` — **public key**. Goes into
  `src-tauri/tauri.conf.json` under `plugins.updater.pubkey`. Safe
  to commit; the app uses it to verify update signatures.

**2. Set GitHub secrets:**

In repo settings → Secrets and variables → Actions, add:
- `TAURI_SIGNING_PRIVATE_KEY` — the contents of `~/.tauri/vista.key`
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — the password you set when
  generating the key (empty if you skipped it)

**3. Paste the pubkey into tauri.conf.json:**

Replace `REPLACE_WITH_PUBLIC_KEY_FROM_TAURI_SIGNER_GENERATE` in
`plugins.updater.pubkey` with the contents of `~/.tauri/vista.key.pub`
(one line).

## Cut a release

Bump the version in **three** files, keeping them identical:
`package.json`, `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml`.
Miss `Cargo.toml` and the release fails — `scripts/check-versions.mjs`
compares all three, and `release.yml` runs it before the slow Rust
build. `Cargo.lock` is gitignored, so there's nothing to regenerate.

```bash
node scripts/check-versions.mjs v0.2.0   # verify locally first
git add -A && git commit -m "Release v0.2.0"
git tag v0.2.0
git push origin master
git push origin v0.2.0
```

The `release.yml` workflow runs on tag push. ~10 min later you'll have
a draft GitHub Release with:
- Windows: `Vista Platform_0.2.0_x64_en-US.msi` + `Vista Platform_0.2.0_x64-setup.exe`
- `latest.json` — the updater manifest

Mac builds are commented out in `release.yml` until Apple signing and
notarization are set up; without a cert the build fails every release.

Edit the draft, add release notes, publish. Every installed user gets
an auto-update prompt on next launch.

## Microsoft Store submission (when ready)

1. **Microsoft Partner Center account** — $19 one-time for individual,
   $99 one-time for company. https://partner.microsoft.com/dashboard
2. **Reserve the app name** — "Vista Platform" or similar
3. **Build the MSIX** — Tauri's `msi` target produces an MSI, but the
   Store wants MSIX. Convert with the **MSIX Packaging Tool** (free
   from Microsoft) or rebuild with `tauri build --bundles msix` once
   we add that target.
4. **Microsoft signs the package for you** when you upload through
   Partner Center — no EV cert needed.
5. **Submit** — fill in the listing (description, screenshots, age
   rating, price = free or paid), upload the MSIX, wait ~7 days for
   review.
6. **Updates** — every new MSIX you upload triggers a Store-managed
   update for all installed users. The built-in Tauri updater becomes
   redundant for Store users (still useful for sideloaded installs).

The $300 EV cert this whole architecture lets you skip is only needed
if you distribute the `.exe` directly from your own website without
going through the Store.

## Project layout

```
.
├── src/                              # Title-bar frontend (HTML/CSS/TS)
│   ├── index.html                    # Title bar markup + drag region
│   ├── styles.css                    # Chrome styling — champagne ground,
│   │                                   matches the iPad app palette
│   └── main.ts                       # Wires the min/max/close buttons
│                                       to Rust IPC commands
├── src-tauri/
│   ├── src/
│   │   ├── main.rs                   # Entry point — calls lib.rs::run()
│   │   └── lib.rs                    # ★ The whole shell:
│   │                                   #   - borderless window
│   │                                   #   - two child webviews (title
│   │                                   #     bar + content)
│   │                                   #   - resize handler
│   │                                   #   - window-control IPC commands
│   ├── capabilities/default.json     # Tauri 2 IPC permissions whitelist
│   ├── icons/                        # Generated by `npm run tauri icon`
│   ├── tauri.conf.json               # App metadata, window config,
│   │                                   updater endpoint, bundle targets
│   ├── Cargo.toml                    # Rust dependencies
│   └── build.rs                      # Tauri build-time codegen
├── .github/workflows/release.yml     # Tag-driven multi-OS build → release
├── package.json                      # Tauri CLI + JS API deps
├── tsconfig.json                     # For the title-bar TS file
└── README.md                         # You are here
```

## Things to think about before the Store submission

- **Branding**: confirm the Vista mark works at 16×16 (title bar
  logo) and 256×256 (taskbar / dock). The current placeholder is a
  blue rounded square with a "V" — replace before shipping.
- **Custom URL scheme** — register `vista://` so password-reset emails
  open in the desktop app instead of the system browser. Tauri has a
  deep-link plugin (`tauri-plugin-deep-link`) for this.
- **Crash + usage telemetry** — Sentry has an official Tauri SDK.
  Worth wiring before public launch so you see real-world crashes
  with stack traces.
- **System tray** — useful for "minimize to tray + still notify on
  new messages." Tauri supports it via the `tray-icon` feature
  (already enabled in `Cargo.toml`). The Rust code is ~30 lines.
- **Offline behavior** — when the user has no network, WebView2 shows
  its default "no internet" page. A nicer experience: detect via
  the content webview's `error` event and show a branded "we're
  offline" overlay from the title-bar layer. ~half-day add-on.
- **Auto-launch on system startup** — `tauri-plugin-autostart`. Some
  medspas may want this for the kiosk/front-desk machine.

## Versioning

Two version strings need to stay in lockstep:
- `package.json` → `"version"`
- `src-tauri/tauri.conf.json` → `"version"`

The release workflow uses the `tauri.conf.json` value for the bundle
name + the GitHub Release tag. Mismatched versions don't crash but
make release artifacts confusing.
