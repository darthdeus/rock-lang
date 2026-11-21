//! Synthetic braceless blocks must not win the node lookup at their body's first and last char.

use crate::fixture::{PATH, position_of_offset, workspace};

const PREAMBLE: &str = "struct MarkerS { f: int }\nenum MarkerE { AAA, BBB }\n";
