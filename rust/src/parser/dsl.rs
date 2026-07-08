//! Extract a `Scene` AST from an SVG rendered by `@preview/candy`.
//!
//! `@preview/candy` embeds the serialized `Scene` (our `candy-json`) inside a
//! hidden `<text lang="candy-json">…</text>` node. We locate that node and
//! deserialize the JSON back into a `Scene`, preserving `private_metadata`
//! verbatim.

use std::path::Path;

use crate::core::ast::Scene;
use crate::core::error::CandyError;

/// Extract a `Scene` AST from an SVG rendered by `@preview/candy`.
///
/// Precondition: `svg_path` is a valid SVG file.
/// Postcondition: returns `Ok(Scene)` with matching `private_metadata`.
///
/// NOTE: the spec says "use `usvg` to parse the SVG". For the first version we
/// use a targeted XML scan for the `lang="candy-json"` node instead (a full
/// `usvg` parse is a reserved optimization; the JSON itself is authoritative).
pub fn extract_dsl_from_svg(svg_path: &Path) -> Result<Scene, CandyError> {
    let svg = std::fs::read_to_string(svg_path)?; // E001 on missing file

    let marker = svg
        .find("lang=\"candy-json\"")
        .ok_or_else(|| CandyError::Dsl("no candy-json block found".into()))?; // E003

    let open_tag_start = svg[..marker]
        .rfind("<text")
        .ok_or_else(|| CandyError::Dsl("candy-json not inside a <text>".into()))?; // E003
    let open_tag_end = svg[open_tag_start..]
        .find('>')
        .map(|k| open_tag_start + k)
        .ok_or_else(|| CandyError::Dsl("malformed <text>".into()))?; // E003
    let close = svg[open_tag_end..]
        .find("</text>")
        .map(|k| open_tag_end + k)
        .ok_or_else(|| CandyError::Dsl("unterminated <text>".into()))?; // E003

    let content = &svg[open_tag_end + 1..close];
    let scene: Scene = serde_json::from_str(content)?; // E003 on invalid JSON
    scene.validate().map_err(CandyError::Parse)?; // E002
    Ok(scene)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ast::{Action, Label, Scene, Slide};
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;

    #[test]
    fn round_trips_through_json() {
        let scene = Scene {
            slides: vec![Slide {
                duration_frames: 12,
                actions: vec![Action::MoveTo {
                    target: Label("x".into()),
                    to: (1.0, 2.0),
                }],
            }],
            items: {
                let mut m = HashMap::new();
                m.insert(Label("x".into()), "circle()".into());
                m
            },
            initial: HashMap::new(),
            audio: Vec::new(),
            private_metadata: PrivateMeta::default(),
        };
        let json = serde_json::to_string(&scene).unwrap();
        let svg = format!(
            "<svg><text lang=\"candy-json\">{json}</text></svg>"
        );
        let tmp = std::env::temp_dir().join("candy_test_dsl.svg");
        std::fs::write(&tmp, svg).unwrap();
        let back = extract_dsl_from_svg(&tmp).unwrap();
        assert_eq!(back.slides[0].duration_frames, 12);
        assert_eq!(back.private_metadata.version_codename, "Orange Candy");
        std::fs::remove_file(&tmp).ok();
    }
}
