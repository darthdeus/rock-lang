use crate::common::{emit_module, mangled_name, run_luajit};

const ON_KILLED: &str = r#"
enum Ev { Tick(int) }
global LOG: int = 0
fn note(x: int) { LOG = x }

co fn worker(seed: int) -> Coroutine<Ev> {
    let base = seed + 100
    on_killed(() => note(base))
    yield Ev::Tick(base)
}

fn main() {
    let c = worker(0)
    let _ = c.resume()
    c.kill()
}
"#;
