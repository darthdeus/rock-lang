use crate::common::{emit_module, function_body, run_main};

#[test]
fn nullable_map_coalesce_keeps_only_the_insert_box() {
    for (fields, init, fallback, ctor, expected) in [
        (
            "a: int, b: int",
            "a: seed, b: seed + 1",
            "a: 0, b: 0",
            "P_ct(",
            0,
        ),
        (
            "a: int, b: string",
            "a: seed, b: \"x\"",
            "a: 0, b: \"fallback\"",
            "P_vt(",
            1,
        ),
    ] {
        let lua = emit_module(&format!(
            r#"
value P {{ {fields} }}
@nomangle fn round_trip(seed: int) -> int {{
    let values: Map<string, P?> = map_new()
    values["p"] = P {{ {init} }}
    let p = values.get("p") ?? P {{ {fallback} }}
    return p.a
}}
fn main() {{ print(round_trip(7)) }}
"#,
        ));
        let body = function_body(&lua, "round_trip");
        assert_eq!(body.matches(ctor).count(), expected);
        assert_eq!(run_main(&lua), "7");
    }
}

const PIPELINE: &str = r#"
nobox value Pipeline {
    shader_name: string,
    parametric_quads: bool = false,
    after_scene: bool = false,
}
"#;

#[test]
fn nullable_pipeline_transfers_do_not_materialize() {
    let fixture = r#"
struct RecipeHolder { pipeline: Pipeline? = null }
fn publish(h: RecipeHolder, p: Pipeline) { h.pipeline = p }
fn copy_recipe(dst: RecipeHolder, src: RecipeHolder) { dst.pipeline = src.pipeline }
fn clear_recipe(h: RecipeHolder) { h.pipeline = null }
fn cached_pipeline() -> Pipeline {
    static p = materialize(Pipeline { shader_name: "ui" })
    p
}
@nomangle inline(lua) fn __out(s: string) = "io.write(s, ' ')"
fn describe(h: RecipeHolder) {
    if let p = h.pipeline {
        __out("{p.shader_name} {p.parametric_quads} {p.after_scene}")
    } else { __out("empty") }
}
fn main() {
    let src = RecipeHolder {}
    let dst = RecipeHolder {}
    publish(src, Pipeline { shader_name: "world", parametric_quads: true, after_scene: true })
    copy_recipe(dst, src)
    clear_recipe(src)
    describe(src)
    describe(dst)
    copy_recipe(dst, src)
    describe(dst)
    publish(src, cached_pipeline())
    copy_recipe(dst, src)
    describe(dst)
}
"#;
    let lua = emit_module(&format!("{PIPELINE}\n{fixture}"));
    for name in ["publish", "copy_recipe"] {
        let body = function_body(&lua, name);
        for ctor in ["Pipeline_vt(", "Pipeline_ct("] {
            assert!(!body.contains(ctor));
        }
    }
    assert_eq!(run_main(&lua), "empty world true true empty ui false false");
}

#[test]
fn nullable_field_emptying_does_not_rebox_the_value() {
    let lua = emit_module(include_str!(
        "../../../tests/noalloc/nullable_value_join_sroa.rock"
    ));
    let body = function_body(&lua, "poll_in");
    for ctor in ["Tk_vt(", "Tk_ct(", "Ent_vt(", "Ent_ct("] {
        assert!(!body.contains(ctor), "{body}");
    }
}
