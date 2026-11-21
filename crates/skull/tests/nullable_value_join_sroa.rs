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
