---
title: feat: Tags on preview panel + Chinese font support
type: feat
status: active
date: 2026-07-26
---

# feat: Tags on Preview Panel + Chinese Font Support

## Summary

Show auto-generated tags at the top of the preview panel when a file is selected, and add CJK font support so Chinese (simplified/traditional) tags render correctly.

---

## Requirements

- R1. When a file is selected (from search results or file browser), its auto-tags display at the top of the preview panel
- R2. Chinese characters (simplified and traditional) render correctly in tags, file names, and all UI text
- R3. Tag display in preview panel shows all tags, not just first 5 (current file browser inline display limiting)

---

## Implementation Units

### U1. Show tags at top of preview panel

**Goal:** When `selected_hash` is set (from search click or file browser browse), query `auto_tag_status` and render tags at the top of the right preview column.

**Files:** `src/app.rs`

**Approach:**
- In the right column (before preview texture/text/empty states), check if `selected_hash` is Some
- If yes, query `tag_store.auto_tag_status(selected_hash)` for tags
- Render tags in a horizontal wrap layout using `RichText` with tag chips
- Reuse the sparkle icon and color from the file browser inline display

**Test scenarios:**
- Click a search result → tags appear at top of preview panel
- Browse a file from file browser → tags appear at top of preview panel
- No file selected → no tags shown, normal empty state
- File without tags → no tags section shown

### U2. Add CJK font for Chinese character support

**Goal:** Bundle a CJK-capable font so Chinese text renders correctly in egui.

**Files:** `src/main.rs`, `Cargo.toml`

**Approach:**
- Use `eframe::NativeOptions` with a font definition that falls back to a CJK font
- The simplest approach: use `egui::FontDefinitions` with a CJK font added via `font_data`
- Since we can't bundle a large font file, use `egui-winit` with system font fallback or embed a small CJK font
- Alternative: Use `egui::FontData::from_owned()` with a bundled Noto Sans SC .ttf file
- If bundling isn't feasible, use `eframe`'s built-in approach to register custom fonts via `cc.egui_ctx.set_fonts()`

**Test scenarios:**
- Chinese tags from AI (like 签证邀请函, 委托书) render without tofu/boxes
- Mixed Chinese+English tags display correctly

## Sources

- egui 0.30 font customization: `egui::FontDefinitions`, `FontData`
- Existing tag display: `src/app.rs` line 1363 (file browser inline)
- Preview panel: `src/app.rs` line 1540+ (right column rendering)
