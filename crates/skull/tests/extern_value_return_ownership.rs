use crate::common::{emit_module, function_body};

const EXTERN_VALUE_RETURN: &str = r#"
value Stats { a: int, b: int }

extern(ffi, "host") fn host_probe_stats() -> Stats

fn main() {
    let s = host_probe_stats()
    print("{s.a}")
}
"#;
