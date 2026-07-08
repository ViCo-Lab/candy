/// Create a new animation scene.
///
/// - width (length): canvas width
/// - height (length): canvas height
/// - fps (int): frames per second
/// -> dictionary: a `Scene` value
#let scene(width: 1280pt, height: 720pt, fps: 30) = {
  (
    kind: "scene",
    width: width,
    height: height,
    fps: fps,
    mobjects: (),
    animations: (),
  )
}

/// Render the scene using the Rust backend.
///
/// - s (dictionary): a `Scene` produced by `scene()`
/// - output (string): output directory for rendered frames
#let render(s, output: "out") = {
  // Signature only — delegates to the Rust `candy render` command.
  none
}
