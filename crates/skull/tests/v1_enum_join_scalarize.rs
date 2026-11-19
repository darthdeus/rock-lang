use crate::common::{emit_module, emit_v1_package, function_body, mangled_name};

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
    assert!(!body.contains("{__variant"));

    let v1_src = format!(
        "{}\npub fn probe() -> int {{ return begin(1.0).n }}\nfn main() {{}}",
        SRC
    );
    let emitted = emit_v1_package("v1join", &[("main.rock", &v1_src)]);
    let body = function_body(&emitted.lua, &mangled_name(&emitted.lua, "begin"));
    assert!(!body.contains("{__variant"));
    assert!(!body.contains("\"v1join::"));
}
