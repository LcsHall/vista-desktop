# Tauri icons

This directory is empty in the repo. Tauri's CLI generates every
icon size from a single 1024×1024 PNG:

```bash
npm run tauri icon path/to/vista-source.png
```

This emits the full set Tauri needs:

- `32x32.png`
- `128x128.png`
- `128x128@2x.png` (256×256)
- `icon.icns` — macOS bundle
- `icon.ico` — Windows installer
- Several Windows Store sizes (`Square*Logo.png`, `StoreLogo.png`)

**Source PNG requirements:**
- 1024×1024
- Transparent background OR a colored background that contrasts
  with both light and dark macOS dock backgrounds
- Padding around the mark: the macOS dock crops to a squircle, so
  leave ~10% padding so nothing important gets clipped

Until icons exist here, `npm run tauri dev` shows a Tauri default
icon (purple cube). The dev experience still works fine; just the
window/taskbar icon is the placeholder.

For the Microsoft Store submission, the WiX/NSIS installer plus the
MSIX needs the matching `Square*Logo.png` sizes — `tauri icon`
generates all of them in one pass.
