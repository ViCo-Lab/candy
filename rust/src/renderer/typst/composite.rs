//! Formula-id localization for the per-glyph `#transform` overlay path.

/// Rewrite every `id="X"` in `markup` to `id="{prefix}X"` and every
/// `xlink:href="#X"` / `href="#X"` to the prefixed form, so two formulas that
/// both define `glyph0`, … can be embedded in the same SVG document without
/// their symbol definitions colliding.
pub(crate) fn localize_formula_ids(markup: &str, prefix: &str) -> String {
    // Collect all ids defined in this markup.
    let mut ids: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(pos) = markup[i..].find("id=\"") {
        let start = i + pos + 4;
        if let Some(end) = markup[start..].find('"') {
            ids.push(markup[start..start + end].to_string());
            i = start + end + 1;
        } else {
            break;
        }
    }
    // Longest first so an id that is a prefix of another is rewritten after it.
    ids.sort_by_key(|id| std::cmp::Reverse(id.len()));
    let mut out = markup.to_string();
    for id in &ids {
        out = out.replace(&format!("id=\"{id}\""), &format!("id=\"{prefix}{id}\""));
        out = out.replace(
            &format!("xlink:href=\"#{id}\""),
            &format!("xlink:href=\"#{prefix}{id}\""),
        );
        out = out.replace(
            &format!("href=\"#{id}\""),
            &format!("href=\"#{prefix}{id}\""),
        );
    }
    out
}
