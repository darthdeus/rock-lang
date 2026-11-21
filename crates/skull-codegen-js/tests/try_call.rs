//! tests/library/js_runtime.rock runs try_call and unused checked index
//! reads on both backends. Here an unused read through a foreign Map or
//! Vec must still enter its host getter.

use crate::common::ws_with;

const TRY_CALL: &str = "@intrinsic @nomangle inline(lua) fn try_call(f: fn()) -> bool = \"\"\"\n    local ok, err = pcall(f)\n    return not ok\n\"\"\"\n";
