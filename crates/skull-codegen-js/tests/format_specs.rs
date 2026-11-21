//! Code sinking must not move a side effect ahead of a format or checked
//! numeric operation that can throw: each probe counts the marks that ran
//! before the error and compares with the unsunk order. Format-spec output is
//! tests/strings/format_specs.rock.

use crate::common::ws_with;
