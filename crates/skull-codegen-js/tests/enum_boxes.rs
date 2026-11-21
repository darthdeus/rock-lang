//! Flat-enum inspect metadata on JS. Emitted programs only ever build exact
//! payload layouts (tests/patterns/js_enum_boxes.rock runs those on both
//! backends); a box from opaque JS can carry a stray slot, a colliding named key
//! or an inactive trailing descriptor, and the runtime must then keep every label
//! positional rather than partially rename it.

use crate::common::ws_with;
