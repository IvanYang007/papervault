# Auto-Tagging — Manual Test Guide

## Prerequisites

1. **DeepSeek API key** — https://platform.deepseek.com → create key
2. **Set env var** (PowerShell as Admin):
   ```powershell
   [Environment]::SetEnvironmentVariable("DEEPSEEK_API_KEY", "sk-your-key-here", "User")
   ```
   Restart terminal.

## Build & Test

```powershell
cd D:\Github\papervalt2
git checkout feat/auto-tagging-deepseek
cargo test                    # Expect 93 passed, 0 failed
cargo build --release
```

## Start the App

```powershell
.\target\release\papervault.exe
```

> Needs `pdfium.dll` next to the exe. Get from [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries/releases), extract `pdfium-win-x64.tgz` → `bin/pdfium.dll`.

## Test Flow

### 1. Folder import with auto-tag opt-in

1. Click **"Add Folder"**
2. Enter or browse to a folder with readable PDFs
3. **Check ✅ "Enable AI auto-tagging (DeepSeek)"**
4. Read the privacy notice
5. Click **"Set Folder"**

Indexing starts. The auto-tagger thread activates in the background.

### 2. Verify status indicator

Open the tag panel (☰ → Tags). At the top you should see:

- ☁ **Auto-tag: ready** (green) — thread is running, idle
- ☁ **Auto-tag: running...** (blue) + progress bar — batch in progress
- ☁ **Auto-tag: error** (red) — API issue, with dismiss button

### 3. View auto-tags

1. Search for a document in your imported folder
2. **Click** a search result to select it
3. Look below the manual tags section:

```
─────────────────
✨ Auto-tags

┌──────────────────────┐  ← dashed = not accepted
│ ✨ tax-return    [✕] │
└──────────────────────┘
┌──────────────────────┐  ← solid green = accepted
│ ✨ tax           [✕] │
└──────────────────────┘
👤 yang guorui           ← entity tags
👤 guorui yang
📅 2023
📄 1040
```

- **Click** a tag → toggles accepted (dashed ↔ green solid)
- **Hover** → tooltip with document filename
- **✕** → permanently dismisses the tag

### 4. Entity type icons

| Icon | Entity Type | Example |
|------|-------------|---------|
| 👤 | Person name | yang guorui |
| 🏢 | Organization | IRS |
| 📅 | Year | 2023 |
| 📄 | Document ID | 1040 |
| 💰 | Amount | $45,230 |

### 5. Search verification

- Search **"yang guorui"** → finds docs tagged with Yang Guorui (including "Guorui Yang" variants)
- Search **"tax 2023"** → finds tax documents from 2023
- Search **"yangguorui"** → CJK concatenated variant also matches

### 6. Cache verification

1. Import a folder → tags generate (check logs for "AI tagged")
2. **Re-import the same folder** → no API calls (check logs for "cache hit")

### 7. Error handling

- Delete `%APPDATA%\papervault\auto_tag.json` → auto-tagging stops gracefully
- Unset `DEEPSEEK_API_KEY` → status turns red with error
- Import image-only PDF → skipped (logged, no crash)

## Logs

```powershell
$env:RUST_LOG="papervault=debug"
.\target\release\papervault.exe
```

Key log messages:
- `cache hit (tier 1) for ...` — exact match, zero API cost
- `cache hit (tier 2) for ...` — filename-token match, zero API cost
- `AI tagged ...: N tags` — DeepSeek API called
- `auto-tag failed for ...` — API error, check retry count

## Feature Checklist

| Feature | Test |
|---------|------|
| [ ] 93 tests pass | `cargo test` |
| [ ] App starts | Launch exe |
| [ ] Opt-in checkbox on import | Folder picker dialog |
| [ ] Privacy disclosure visible | Text below checkbox |
| [ ] Status indicator works | ☁ green/blue/red in tag panel |
| [ ] Progress bar shown | During batch processing |
| [ ] Auto-tags appear | Sparkle icons in tag panel |
| [ ] Entity type icons | 👤🏢📅📄💰 render correctly |
| [ ] Click to accept (dashed→solid) | Tag toggles visual state |
| [ ] Hover tooltip | Shows filename |
| [ ] ✕ dismiss works | Tag removed, stays gone |
| [ ] Search matches tags | "yang guorui" finds tag variants |
| [ ] Cache hit on re-import | No duplicate API calls |
| [ ] Error handling | Red status on API failure |
