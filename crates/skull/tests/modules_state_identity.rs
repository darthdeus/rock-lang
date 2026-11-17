use crate::common::{emit_v1_package, run_luajit};

#[test]
fn module_storage_and_statics_survive_fragment_moves_and_full_reload() {
    let declarations = "global COUNT: int = 0\nconst LIMIT: int = 7\n";
    let functions = r#"
pub fn bump() { COUNT += 1 }
pub fn read() -> int { return COUNT }
pub fn limit() -> int { return LIMIT }
pub fn tick() -> int {
    static n = 0
    n += 1
    fn twice(x: int) -> int { return x * 2 }
    return twice(n)
}
"#;
    let emit = |moved: bool| {
        let front = format!(
            "mod a;\n{}{functions}",
            if moved { "" } else { declarations }
        );
        let back = format!("mod a;\n{}", if moved { declarations } else { "" });
        emit_v1_package("v1state", &[
            ("main.rock", r#"
@nomangle fn probe() {
    a::bump(); a::bump(); b::bump()
    print("a={a::read()} b={b::read()} limit={a::limit()} ticks={a::tick()},{a::tick()}")
}
fn main() {}
"#),
            ("a1.rock", &front),
            ("a2.rock", &back),
            ("b.rock", "mod b;\nglobal COUNT: int = 0\npub fn bump() { COUNT += 1 }\npub fn read() -> int { return COUNT }"),
        ]).lua
    };
    let program = format!(
        "{}\nprobe()\nHOT_RELOAD = true\n{}\nprobe()\n",
        emit(false),
        emit(true)
    );
    assert_eq!(
        run_luajit(&program),
        "a=2 b=1 limit=7 ticks=2,4\na=4 b=2 limit=7 ticks=6,8"
    );
}
