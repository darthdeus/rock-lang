use crate::common::{emit_module, inline_off_guard, run_main};

const BOUNDED: &str = r#"
fn total<I: Iterator<int>>(it: I) -> int {
    let s = 0
    for v in it { s = s + v }
    return s
}

fn main() {
    let xs: [int] = [1, 2, 3]
    print("{total(xs.iter())}")
}
"#;

const NESTED: &str = r#"
struct Item { id: int, p: float }

fn main() {
    let xs = [
        Item { id: 1, p: 3.0 },
        Item { id: 2, p: 2.0 },
    ]
    let ids = xs.map(_.id).collect()
    print("{ids[0]}")
}
"#;
