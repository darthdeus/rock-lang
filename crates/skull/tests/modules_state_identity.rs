use crate::common::{V1Fragment, emit_v1_package, module_id, run_main};

const MAIN: &str = r#"
fn main() {
    a::bump()
    a::bump()
    b::bump()
    print("a={a::read()} b={b::read()} limit={a::limit()}")
}
"#;

const MOD_A: &str = r#"
mod a;

global COUNT: int = 0

const LIMIT: int = 7

pub fn bump() {
    COUNT = COUNT + 1
}

pub fn read() -> int {
    return COUNT
}

pub fn limit() -> int {
    return LIMIT
}
"#;

const MOD_B: &str = r#"
mod b;

global COUNT: int = 0

pub fn bump() {
    COUNT = COUNT + 1
}

pub fn read() -> int {
    return COUNT
}
"#;

fn package() -> skull::mir_emit::MirEmit {
    emit_v1_package(
        "v1state",
        &[
            V1Fragment {
                file: "main.rock",
                src: MAIN,
            },
            V1Fragment {
                file: "a.rock",
                src: MOD_A,
            },
            V1Fragment {
                file: "b.rock",
                src: MOD_B,
            },
        ],
    )
}

#[test]
fn v1_module_globals_have_independent_storage() {
    let emitted = package();
    let out = run_main(&emitted.lua);
    assert_eq!(out, "a=2 b=1 limit=7");
}

#[test]
fn v1_global_state_identity_is_its_def_key() {
    let emitted = package();
    assert!(
        emitted
            .lua
            .contains("identity=\"global:v1state::a::COUNT\"")
    );
    assert!(
        emitted
            .lua
            .contains("identity=\"global:v1state::b::COUNT\"")
    );
    assert!(!emitted.lua.contains("global:a.rock::"));
}

#[test]
fn v1_global_identity_survives_a_move_between_fragments() {
    let front = r#"
mod a;

global COUNT: int = 0

pub fn bump() {
    COUNT = COUNT + 1
}
"#;
    let back = r#"
mod a;

pub fn read() -> int {
    return COUNT
}
"#;
    let moved_front = r#"
mod a;

pub fn bump() {
    COUNT = COUNT + 1
}
"#;
    let moved_back = r#"
mod a;

global COUNT: int = 0

pub fn read() -> int {
    return COUNT
}
"#;
    let main = r#"
fn main() {
    a::bump()
    print("a={a::read()}")
}
"#;
    let emit = |front: &str, back: &str| {
        emit_v1_package(
            "v1state",
            &[
                V1Fragment {
                    file: "main.rock",
                    src: main,
                },
                V1Fragment {
                    file: "a1.rock",
                    src: front,
                },
                V1Fragment {
                    file: "a2.rock",
                    src: back,
                },
            ],
        )
    };
    let before = emit(front, back);
    let after = emit(moved_front, moved_back);
    let identity = "identity=\"global:v1state::a::COUNT\"";
    assert!(before.lua.contains(identity));
    assert!(after.lua.contains(identity));
}

#[test]
fn v1_code_identity_uses_module_scope_and_signature() {
    let fragments = [
        V1Fragment {
            file: "main.rock",
            src: MAIN,
        },
        V1Fragment {
            file: "a.rock",
            src: MOD_A,
        },
        V1Fragment {
            file: "b.rock",
            src: MOD_B,
        },
    ];
    let ws = crate::common::ws_v1_package("v1state", &fragments);
    let mid = module_id(&ws, "main.rock");
    let emitted = skull::mir_emit::emit_workspace_mir(&ws, mid).expect("emit");
    let interner = skull_mir::monoty::MonoInterner::default();
    let bump = ws
        .modules
        .iter()
        .filter(|module| module.file.as_str() == "a.rock")
        .find_map(|module| {
            module
                .functions
                .iter()
                .find(|(_, f)| f.name.as_str().ends_with("bump"))
                .map(|(fid, _)| {
                    skull_mir::code_identity::CodeIdentity::Named(
                        skull_mir::instance::Instance::base(skull_mir::mir::FuncRef {
                            module: module.id,
                            func: fid,
                        }),
                    )
                })
        })
        .expect("a::bump");
    assert!(emitted.code_artifact.entries.contains_key(&bump));
    assert_eq!(
        bump.logical_key(&ws, &interner),
        "named:v1state::a::bump_1c91355d_0<>"
    );
}

#[test]
fn v1_static_identity_is_keyed_on_its_owner_code_key() {
    let emitted = emit_v1_package(
        "v1state",
        &[
            V1Fragment {
                file: "main.rock",
                src: "fn main() {\n    print(\"t={a::tick()}{a::tick()}\")\n}\n",
            },
            V1Fragment {
                file: "a.rock",
                src: "mod a;\n\npub fn tick() -> int {\n    static n: int = 0\n    n = n + 1\n    return n\n}\n",
            },
        ],
    );
    assert!(
        emitted
            .lua
            .contains("static:named:v1state::a::tick_1c91355d_0<>::n#0")
    );
    let out = run_main(&emitted.lua);
    assert_eq!(out, "t=12");
}

const LOCALS_MAIN: &str = r#"
fn main() {
    print("{a::tick()} {a::tick()}")
}
"#;

const LOCALS_MOD: &str = r#"
mod a;

pub fn tick() -> int {
    static n = 0
    n = n + 1
    fn twice(x: int) -> int {
        return x * 2
    }
    return twice(n)
}
"#;

#[test]
fn v1_static_cells_and_nested_fns_emit_identifiers() {
    let emitted = emit_v1_package(
        "v1loc",
        &[
            V1Fragment {
                file: "main.rock",
                src: LOCALS_MAIN,
            },
            V1Fragment {
                file: "a.rock",
                src: LOCALS_MOD,
            },
        ],
    );
    let out = run_main(&emitted.lua);
    assert_eq!(out, "2 4");
}
