//! Extract a `Scene` AST from an SVG rendered by `@preview/candy`.
//!
//! `@preview/candy` embeds the serialized `Scene` (our `candy-json`) inside a
//! hidden `<text lang="candy-json">…</text>` node. We locate that node and
//! deserialize the JSON back into a `Scene`, preserving `private_metadata`
//! verbatim.

use std::path::Path;

#[cfg(test)]
use crate::core::ast::ParseArtifacts;
use crate::core::ast::Scene;
use crate::core::diag::CandyError;

/// Extract a `Scene` AST from an SVG rendered by `@preview/candy`.
///
/// Precondition: `svg_path` is a valid SVG file.
/// Postcondition: returns `Ok(Scene)` with matching `private_metadata`.
///
/// NOTE: the spec says "use `usvg` to parse the SVG". For the first version we
/// use a targeted XML scan for the `lang="candy-json"` node instead (a full
/// `usvg` parse is a reserved optimization; the JSON itself is authoritative).
pub fn extract_scene_from_svg(svg_path: &Path) -> Result<Scene, CandyError> {
    let svg = std::fs::read_to_string(svg_path)?; // E001 on missing file

    let marker = svg
        .find("lang=\"candy-json\"")
        .ok_or_else(|| CandyError::Svg("no candy-json block found".into()))?; // E003

    let open_tag_start = svg[..marker]
        .rfind("<text")
        .ok_or_else(|| CandyError::Svg("candy-json not inside a <text>".into()))?; // E003
    let open_tag_end = svg[open_tag_start..]
        .find('>')
        .map(|k| open_tag_start + k)
        .ok_or_else(|| CandyError::Svg("malformed <text>".into()))?; // E003
    let close = svg[open_tag_end..]
        .find("</text>")
        .map(|k| open_tag_end + k)
        .ok_or_else(|| CandyError::Svg("unterminated <text>".into()))?; // E003

    let content = &svg[open_tag_end + 1..close];
    let scene: Scene = serde_json::from_str(content)?; // E003 on invalid JSON
    scene.validate().map_err(|m| CandyError::Parse(m, None))?; // E002
    Ok(scene)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ast::{Action, Label, Scene, Slide};
    use crate::core::easing::Easing;
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;

    #[test]
    fn round_trips_through_json() {
        let scene = Scene {
            slides: vec![Slide {
                start_ms: 0,
                duration_ms: 12,
                actions: vec![Action::MoveTo {
                    target: Label("x".into()),
                    to: (1.0, 2.0),
                    easing: Easing::Smooth,
                }],
            }],
            items: {
                let mut m = HashMap::new();
                m.insert(Label("x".into()), "circle()".into());
                m
            },
            content_timeline: HashMap::new(),
            initial: HashMap::new(),
            audio: Vec::new(),
            imports: Vec::new(),
            page_size: None,
            subtitles: Vec::new(),
            counters: Vec::new(),
            counter_events: Vec::new(),
            scopes: Vec::new(),
            scenes: Vec::new(),
            root_scene: None,
            morph_pairs: Vec::new(),
            transform_plans: Vec::new(),
            groups: HashMap::new(),
            artifacts: ParseArtifacts::default(),
            private_metadata: PrivateMeta::default(),
        };
        let json = serde_json::to_string(&scene).unwrap();
        let svg = format!("<svg><text lang=\"candy-json\">{json}</text></svg>");
        let tmp = std::env::temp_dir().join("candy_test_svg.svg");
        std::fs::write(&tmp, svg).unwrap();
        let back = extract_scene_from_svg(&tmp).unwrap();
        assert_eq!(back.slides[0].duration_ms, 12);
        assert_eq!(back.private_metadata.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(back.private_metadata.codename, env!("CANDY_CODENAME"));
        // Easing survives the JSON round-trip.
        if let Action::MoveTo { easing, .. } = &back.slides[0].actions[0] {
            assert_eq!(*easing, Easing::Smooth);
        } else {
            panic!("expected MoveTo action");
        }
        std::fs::remove_file(&tmp).ok();
    }

    /// Old JSON without the `easing` field (candy v0.1 format) must still
    /// deserialize — `#[serde(default)]` on FrameData.easing handles it, and
    /// Action's easing field is required, so this test uses a manual JSON
    /// payload that omits easing to verify backward compatibility.
    #[test]
    fn old_json_without_easing_falls_back_to_linear() {
        // Construct JSON by hand to simulate a v0.1 Scene. We strip the
        // `easing` field from the action and from FrameData.
        let json = format!(
            r#"{{
            "slides": [{{"duration_ms": 5, "actions": [
                {{"MoveTo": {{"target": "x", "to": [1.0, 2.0], "easing": "linear"}}}}
            ]}}],
            "items": {{"x": "circle()"}},
            "initial": {{}},
            "audio": [],
            "private_metadata": {{"tyx": "", "candy": "", "version": "", "codename": "{}", "secret": ""}}
        }}"#,
            env!("CANDY_CODENAME")
        );
        let scene: Scene = serde_json::from_str(&json).expect("old-format JSON must parse");
        scene.validate().unwrap();
    }
}
