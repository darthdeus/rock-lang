//! A value-producing `if` whose arms construct a flat enum joins its leaves in
//! registers: the join never boxes the value into a `{__variant ...}` table
//! only to unpack it again. That holds for a modules-v1 enum exactly as for a
//! legacy one (Stone's `start` builds its first `Wait` this way).

use crate::common::{emit_module, emit_v1_package, function_body, mangled_name, V1Fragment};

const SRC: &str = r#"
enum Wait { Frame, Secs(float), Until(() -> bool, int) }
struct Task { active: Wait, n: int }
fn begin(delay: float) -> Task {
    let active = if delay > 0.0 { Wait::Secs(delay) } else { Wait::Frame }
    return Task { active: active, n: 1 }
}
"#;

#[test]
fn an_if_joined_v1_enum_stays_in_leaves() {
    let legacy = emit_module(&format!("{SRC}\nfn main() {{ let _ = begin(1.0) }}"));
    let body = function_body(&legacy, &mangled_name(&legacy, "begin"));
    assert!(!body.contains("{__variant"), "legacy control boxes the join:\n{body}");

    let v1_src = format!("{}\npub fn probe() -> int {{ return begin(1.0).n }}\nfn main() {{}}", SRC);
    let emitted = emit_v1_package("v1join", &[V1Fragment { file: "main.rock", src: &v1_src }]);
    let body = function_body(&emitted.lua, &mangled_name(&emitted.lua, "begin"));
    assert!(!body.contains("{__variant"), "a v1 enum join boxes its value:\n{body}");
    // The scalarized `__ty` leaf is the runtime tag every box of the type carries.
    assert!(!body.contains("\"v1join::"), "a scalarized leaf spells the DefKey:\n{body}");
}
