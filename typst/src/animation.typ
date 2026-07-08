/// Play an animation on the given mobject.
///
/// - m (dictionary): target mobject
/// - kind (string): animation kind, e.g. `"fade-in"`, `"show"`
/// - duration (float): duration in seconds
/// -> dictionary: an `AnimationClip` value
#let play(m, kind: "show", duration: 1.0) = {
  (kind: "animation", target: m.name, anim: kind, duration: duration)
}

/// Create a transform animation between two mobjects.
///
/// - a (dictionary): source mobject
/// - b (dictionary): target mobject
/// - duration (float): duration in seconds
/// -> dictionary: an `AnimationClip` value
#let transform(a, b, duration: 1.0) = {
  (
    kind: "animation",
    anim: "transform",
    source: a.name,
    target: b.name,
    duration: duration,
  )
}
