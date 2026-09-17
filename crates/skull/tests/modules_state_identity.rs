//! modules-v1 runtime identity (plan M4 slice 2). A v1 definition's identity is
//! its DefKey (`<package>::<module path>::<name>`), which has two consequences
//! the backend has to honor:
//!
//! - The key is not a Lua identifier, so every identifier the backend mints for
//!   a global -- the global itself, its `__skull_ginit_` initializer -- must be
//!   the key's identifier form, exactly as M4 slice 1 did for functions.
//! - The key is file-independent, so state identity is keyed on the LOGICAL
//!   module: moving a definition between two fragments of one module keeps its
//!   state, and two same-named globals in different modules never share it.
//!   `(file, name)` gets the second half right only by accident (the DefKey is
//!   already in the name) and the first half wrong.
//!
//! These execute the emitted chunk rather than grepping it: a symbol that is
//! wrong in either the definition or a use fails to parse or reads nil.

use crate::common::{emit_v1_package, run_luajit, V1Fragment};

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
            V1Fragment { file: "main.rock", src: MAIN },
            V1Fragment { file: "a.rock", src: MOD_A },
            V1Fragment { file: "b.rock", src: MOD_B },
        ],
    )
}

#[test]
fn v1_module_globals_have_independent_storage() {
    let emitted = package();
    let out = run_luajit(&format!("{}\nmain()\n", emitted.lua), "modules_v1_state");
    assert_eq!(
        out, "a=2 b=1 limit=7",
        "each module's `COUNT` must have its own storage, and a const global must be readable"
    );
}

#[test]
fn v1_global_state_identity_is_its_def_key() {
    let emitted = package();
    assert!(
        emitted.lua.contains("identity=\"global:v1state::a::COUNT\""),
        "a v1 global's StateSchema identity is its DefKey"
    );
    assert!(
        emitted.lua.contains("identity=\"global:v1state::b::COUNT\""),
        "two modules' same-named globals keep distinct identities"
    );
    assert!(
        !emitted.lua.contains("global:a.rock::"),
        "the owning FRAGMENT must not be part of a v1 global's identity"
    );
}

/// The same logical module split across two fragments, with the global moved
/// from one to the other. A file-keyed identity changes here, which would drop
/// the live value on a hot reload and make the two programs' state images
/// incompatible for no source-level reason.
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
                V1Fragment { file: "main.rock", src: main },
                V1Fragment { file: "a1.rock", src: front },
                V1Fragment { file: "a2.rock", src: back },
            ],
        )
    };
    let before = emit(front, back);
    let after = emit(moved_front, moved_back);
    let identity = "identity=\"global:v1state::a::COUNT\"";
    assert!(
        before.lua.contains(identity),
        "the global's identity is its DefKey while it lives in the first fragment"
    );
    assert!(
        after.lua.contains(identity),
        "moving the global to a sibling fragment of the SAME module must not change its identity"
    );
}

/// A v1 function's code identity is its DefKey too: the hot-reload code slot of
/// a function that moves between fragments of one module must not change.
#[test]
fn v1_code_identity_is_its_def_key() {
    let emitted = package();
    assert!(
        emitted.lua.contains("\"named:v1state::a::bump<>\""),
        "a v1 function's logical code identity is its DefKey"
    );
    assert!(
        !emitted.lua.contains("named:a.rock::"),
        "the owning FRAGMENT must not be part of a v1 function's code identity"
    );
}

/// A `static` inside a v1 function is keyed on its owning callable, so the same
/// rule reaches it through the owner's logical key.
#[test]
fn v1_static_identity_is_keyed_on_its_owner_def_key() {
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
        emitted.lua.contains("static:named:v1state::a::tick<>::n#0"),
        "a v1 static is keyed on its owner's DefKey, not on the owner's file"
    );
    let out = run_luajit(&format!("{}\nmain()\n", emitted.lua), "modules_v1_static");
    assert_eq!(out, "t=12", "the static must initialize once and keep its value");
}
