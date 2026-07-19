// Candy — shared constants.
//
// Directional / scale / rotation / visibility constants usable from any candy
// directive argument (e.g. `#animate("a", to: dir-right)` or
// `#spiral-in("a", rotate: full-turn)`).

#let dir-left = (-2.0, 0.0)
#let dir-right = (2.0, 0.0)
#let dir-up = (0.0, -2.0)
#let dir-down = (0.0, 2.0)
#let dir-origin = (0.0, 0.0)
#let dir-up-left = (dir-left.at(0) + dir-up.at(0), dir-left.at(1) + dir-up.at(1))
#let dir-up-right = (dir-right.at(0) + dir-up.at(0), dir-right.at(1) + dir-up.at(1))
#let dir-down-left = (dir-left.at(0) + dir-down.at(0), dir-left.at(1) + dir-down.at(1))
#let dir-down-right = (dir-right.at(0) + dir-down.at(0), dir-right.at(1) + dir-down.at(1))

#let grow = 150%
#let shrink = 50%
#let original = 100%

#let quarter-turn = 90deg
#let half-turn = 180deg
#let full-turn = 360deg

#let visible = 100%
#let half-visible = 50%
#let invisible = 0%
