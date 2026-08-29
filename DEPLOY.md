# Deploy PS2 Backup Tool as Desktop App

## Prerequisites

### All Platforms
- [Node.js](https://nodejs.org/) v18+
- [Rust](https://www.rust-lang.org/tools/install) (via rustup)

### macOS Additional
```bash
xcode-select --install
```

### Windows Additional
- [Microsoft Visual Studio C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
- [WebView2](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) (usually pre-installed on Windows 10+)

## Setup

```bash
# 1. Clone / navigate to project
cd ps2-loader-game

# 2. Install JS dependencies
npm install

# 3. Install Tauri CLI (if not globally installed)
npm install -D @tauri-apps/cli
```

## Development

```bash
# Run in dev mode (hot-reload frontend, Rust recompiles on change)
npx tauri dev
```

This opens the desktop app window. Frontend changes reload instantly; Rust changes trigger recompile (~10-30s first build, ~2s incremental).

## Build for Distribution

```bash
# Build release binary + installer
npx tauri build
```

Output locations:

| Platform | Output |
|----------|--------|
| macOS | `src-tauri/target/release/bundle/dmg/PS2 Backup Tool_0.1.0_aarch64.dmg` |
| Windows | `src-tauri/target/release/bundle/msi/PS2 Backup Tool_0.1.0_x64.msi` |
| Linux | `src-tauri/target/release/bundle/deb/ps2-backup-tool_0.1.0_amd64.deb` |

## Build Flags

```bash
# Debug build (faster compile, larger binary)
npx tauri build --debug

# Build with verbose output
npx tauri build --verbose
```

## Cross-Platform Notes

| Target | Build On | Notes |
|--------|----------|-------|
| macOS ARM (M1+) | macOS | Native |
| macOS Intel | macOS | Add target: `rustup target add x86_64-apple-darwin` |
| Windows | Windows | Or cross-compile from Linux with `cross` |
| Linux | Linux | Ubuntu 22.04+ recommended |

Tauri does **not** support cross-compiling between OSes easily. Build on each target OS natively (or use CI).

## macOS Gatekeeper (Unsigned App)

Without Apple Developer account, macOS will block the app on first launch:

1. Double-click the app → "cannot be opened" dialog appears
2. Go to **System Settings → Privacy & Security**
3. Click **"Open Anyway"** next to the blocked app message
4. Confirm **Open** in the next dialog

Alternatively, right-click the app → **Open** → **Open** (bypasses Gatekeeper once).

## GitHub Actions CI (build all platforms)

The workflow at `.github/workflows/build.yml` builds unsigned binaries automatically on tag push.

### Trigger a release

```bash
# 1. Tag and push
git tag v0.1.0
git push origin v0.1.0
```

This creates a GitHub Release with:
| File | Platform |
|------|----------|
| `PS2-Backup-Tool-macOS-ARM.dmg` | macOS Apple Silicon |
| `PS2-Backup-Tool-macOS-Intel.dmg` | macOS Intel |
| `PS2-Backup-Tool-Windows.msi` | Windows installer |
| `PS2-Backup-Tool-Windows.exe` | Windows portable |

### Manual trigger (no tag)

Go to Actions tab → "Build Desktop App" → "Run workflow"

### No developer account needed

- **macOS**: No Apple Developer account. App is unsigned — user right-clicks > Open on first launch to bypass Gatekeeper.
- **Windows**: No code signing cert. SmartScreen shows warning — user clicks "More info" > "Run anyway".

## Quick Reference

| Command | Purpose |
|---------|---------|
| `npx tauri dev` | Development with hot-reload |
| `npx tauri build` | Production build + installer |
| `npx tauri build --debug` | Debug build |
| `npx tauri icon <path>` | Generate all icon sizes from a 1024x1024 PNG |
| `cargo test` (in src-tauri/) | Run Rust unit tests |
