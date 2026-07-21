# papervault

Fast, lightweight PDF and text file search & viewer for Windows 11. Like old Evernote's search — but local, instant, and built in Rust.

## What it does
- Watches a folder of PDF, .txt, .md, and .log files
- Indexes full text instantly with Tantivy
- Single-window search: type → see results with highlights → click to preview
- Tags (manual v1, AI auto-tagging v2)

## Tech
- egui + eframe for UI
- Tantivy for full-text search
- pdfium for PDF text extraction & rendering
- notify for file system watching
- SQLite for tag storage

## Status
Requirements & design in progress.

