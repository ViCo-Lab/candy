/// Base constructor for a mathematical object (Mobject).
///
/// - name (string): identifier for the object
/// - pos (array): position `[x, y, z]`
/// -> dictionary: an `Mobject`
#let mobject(name: "mobject", pos: (0, 0, 0)) = {
  (kind: "mobject", name: name, pos: pos)
}

/// Set the visibility of an mobject.
///
/// - m (dictionary): target mobject
/// - visible (bool): whether the object is visible
/// -> dictionary: the updated mobject
#let set_visible(m, visible: true) = {
  m
}

/// Move an mobject to a new position.
///
/// - m (dictionary): target mobject
/// - pos (array): new position `[x, y, z]`
/// -> dictionary: the updated mobject
#let move_to(m, pos) = {
  m
}
