use crate::common::{emit_module, function_body, function_params, mangled_name};

fn contains_digit_bridge(hay: &str, needle: &str, tail: &str) -> bool {
    let mut rest = hay;
    while let Some(p) = rest.find(needle) {
        let after = &rest[p + needle.len()..];
        let ndig = after.bytes().take_while(|b| b.is_ascii_digit()).count();
        if ndig > 0 && after[ndig..].starts_with(tail) {
            return true;
        }
        rest = &rest[p + needle.len()..];
    }
    false
}

fn state_capture_field(step: &str, state_param: &str) -> Option<String> {
    let start = step.find("setmetatable({")? + "setmetatable({".len();
    let record = &step[start..];
    let end = record.find("}, __rock_fn_mt)")?;
    record[..end].split(',').find_map(|field| {
        let (name, value) = field.split_once('=')?;
        (value.trim() == state_param).then(|| name.trim().to_string())
    })
}

const PARAM_CAPTURE: &str = r#"
enum Ev { Tick(int) }
global R: int = 0

co fn worker() -> Coroutine<Ev> {
    let counter = 100
    let add = (n: int) => n + counter
    R = add(5)
    yield Ev::Tick(1)
}

fn main() {
    let c = worker()
    let _ = c.resume()
}
"#;

const CAPTURE_FREE: &str = r#"
enum Ev { Tick(int) }
global R: int = 0

co fn worker() -> Coroutine<Ev> {
    let f = () => 42
    R = f()
    yield Ev::Tick(1)
}

fn main() {
    let c = worker()
    let _ = c.resume()
}
"#;

const LOOP_BINDING: &str = r#"
enum Ev { Tick(int) }
global FS: [fn() -> int] = []

co fn looper() -> Coroutine<Ev> {
    for i in 0..3 {
        FS.push(() => i)
        yield Ev::Tick(i)
    }
}

fn main() {
    let c = looper()
    let _ = c.resume()
}
"#;
