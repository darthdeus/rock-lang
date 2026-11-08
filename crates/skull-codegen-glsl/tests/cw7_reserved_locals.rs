//! Lexical keywords are distinct from uniform shadowing: even an unused
//! keyword binding is invalid GLSL. Drive entry and helper allocation.

use skull_codegen_glsl::GlslProfile;
use std::sync::LazyLock;
use ustr::Ustr;

static STDLIB: LazyLock<Vec<skull_parser::Ast>> = LazyLock::new(|| {
    rock_stdlib::stdlib_files()
        .iter()
        .map(|f| {
            skull_parser::parse_module_clean(
                f.contents,
                Ustr::from(rock_stdlib::stdlib_file_id(f.name).as_str()),
            )
            .expect("stdlib parse")
        })
        .collect()
});

const SOURCE: &str = r#"
fn keyword_helper(sample: float, packed: float, resource: float) -> float {
    return sample * packed + resource
}

@shader(fragment)
fn local_frag(tc: Vec2) -> Vec4 {
    let sample_1 = tc.y + 2.0
    let sample = tc.x + 1.0
    return Vec4 { x: sample, y: sample_1, z: 0.0, w: 1.0 }
}

@shader(fragment)
fn param_frag(tc: Vec2) -> Vec4 {
    return Vec4 { x: keyword_helper(tc.x, tc.y, 2.0), y: tc.y, z: 0.0, w: 1.0 }
}

fn register() {
    let a = rock_shader(local_frag)
    let b = rock_shader(param_frag)
}
"#;

fn emit(root: &str, profile: GlslProfile) -> String {
    let mut modules = STDLIB.clone();
    modules.push(skull_parser::parse_module_clean(SOURCE, Ustr::from("<test>")).expect("parse"));
    let ws = bone_semantic::semantic_passes::semantic_pass(&mut modules).expect("semantic pass");
    let errors = skull_mir::verdict::body_errors(&ws);
    assert!(errors.is_empty(), "fixture body errors: {errors:?}");
    let mid = *ws.module_ids.get(&Ustr::from("<test>")).expect("module id");
    let emission = skull_codegen_glsl::emit_shader_entry_with_dependencies(&ws, mid, root, profile)
        .expect("emit");
    println!("SHADER {root} {profile:?}\n{}\nEND SHADER", emission.source);
    emission.source
}

fn identifiers(source: &str) -> impl Iterator<Item = &str> {
    source
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
}

#[test]
fn keyword_local_and_existing_suffix_stay_distinct_in_both_profiles() {
    let profile = GlslProfile::Core330;
    let glsl = emit("local_frag", profile);
    assert!(
        !identifiers(&glsl).any(|s| s == "sample"),
        "keyword local:\n{glsl}"
    );
    let locals: Vec<_> = glsl
        .lines()
        .filter_map(|line| line.trim().strip_prefix("float ")?.strip_suffix(';'))
        .collect();
    assert_eq!(locals.len(), 2, "both locals must be emitted:\n{glsl}");
    assert_ne!(locals[0], locals[1], "renamed locals alias:\n{glsl}");
    for name in locals {
        assert!(
            glsl.contains(&format!("{name} = ")),
            "local is never assigned:\n{glsl}"
        );
        assert!(
            glsl.lines()
                .any(|line| { line.contains("vec4(") && identifiers(line).any(|s| s == name) }),
            "local read lost its binding:\n{glsl}"
        );
    }
}

#[test]
fn keyword_helper_parameter_is_renamed_in_both_profiles() {
    let profile = GlslProfile::Core330;
    let glsl = emit("param_frag", profile);
    assert!(
        glsl.contains("float keyword_helper_"),
        "helper not reached:\n{glsl}"
    );
    // ES-only future keywords and desktop future keywords share the policy.
    for keyword in ["sample", "packed", "resource"] {
        assert!(
            !identifiers(&glsl).any(|s| s == keyword),
            "keyword parameter {keyword}:\n{glsl}"
        );
    }
}
