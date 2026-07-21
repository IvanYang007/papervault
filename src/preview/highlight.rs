use crate::app::HighlightRect;

/// Calculate highlight rectangles by finding search term positions in extracted text.
/// This is a simplified approach — the real implementation uses pdfium's
/// FPDFText_Find* API for pixel-precise bounding boxes.
#[allow(dead_code)]
pub fn find_highlights(
    _page_text: &str,
    _search_terms: &[String],
    _page_width: f32,
    _page_height: f32,
) -> Vec<HighlightRect> {
    // Simplified: return empty highlights.
    // Full implementation requires pdfium text-find API integration
    // to map character positions to pixel coordinates.
    Vec::new()
}

/// Map a character index in page text to approximate pixel coordinates.
#[allow(dead_code)]
fn char_to_rect(
    _char_index: usize,
    _total_chars: usize,
    page_width: f32,
    _page_height: f32,
) -> HighlightRect {
    // Approximate: distribute characters evenly in a grid
    let chars_per_row = (page_width / 8.0) as usize; // ~8px per character
    let row = _char_index / chars_per_row.max(1);
    let col = _char_index % chars_per_row.max(1);

    HighlightRect {
        x: col as f32 * 8.0,
        y: row as f32 * 14.0, // ~14px line height
        w: 8.0,
        h: 14.0,
    }
}
