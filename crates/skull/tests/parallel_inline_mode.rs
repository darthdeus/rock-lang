use std::sync::{Arc, Barrier};
use std::thread;

use crate::common::{InlineOffGuard, emit_module};

const SRC: &str = r#"
@inline fn twice(x: int) -> int { return x + x }
fn main() { print("{twice(3)}") }
"#;

#[test]
fn concurrent_inline_off_and_normal_compiles_keep_their_modes() {
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));

    let off_ready = Arc::clone(&ready);
    let off_release = Arc::clone(&release);
    let off = thread::spawn(move || {
        let _guard = InlineOffGuard::on();
        off_ready.wait();
        let lua = emit_module(SRC);
        off_release.wait();
        lua
    });

    let normal_ready = Arc::clone(&ready);
    let normal_release = Arc::clone(&release);
    let normal = thread::spawn(move || {
        normal_ready.wait();
        let lua = emit_module(SRC);
        normal_release.wait();
        lua
    });

    let off = off.join().expect("inline-off compile thread panicked");
    let normal = normal.join().expect("normal compile thread panicked");
    assert_ne!(off, normal, "the two compile modes produced identical Lua");
    let has_retained_call = |lua: &str| {
        let callee = crate::common::bound_callee_name(lua, "main", "twice_int");
        crate::common::function_body(lua, "main").contains(&format!("r0 = {callee}((3))"))
    };
    assert!(
        has_retained_call(&off),
        "inline-off compilation lost the retained call"
    );
    assert!(
        !has_retained_call(&normal),
        "normal compilation retained the inlineable call"
    );
}
