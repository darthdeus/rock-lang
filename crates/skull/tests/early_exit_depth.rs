use crate::common::{emit_module, run_main};

const EXITS: usize = 260;

/// One body of `EXITS` sequential exits: each step reads `xs[i]` or leaves.
fn chain(step: &str, wrap: &dyn Fn(&str) -> String) -> String {
    let steps: String = (0..EXITS)
        .map(|i| step.replace("{i}", &i.to_string()))
        .collect();
    format!(
        "fn at(xs: [int], k: int) -> int? {{ return xs.get(k) }}\n\
         fn build(xs: [int]) -> int {{\n    let s = 0\n{}    return s\n}}\n\
         fn main() {{\n    let xs = [0]\n    for i in 1..{EXITS} {{ xs.push(i) }}\n    \
         print(build(xs))\n    print(build([0, 1, 2]))\n}}\n",
        wrap(&steps)
    )
}

fn looped(body: &str) -> String {
    format!("    for r in 0..2 {{\n{body}    }}\n")
}

#[test]
fn sequential_early_exits_load_whatever_their_count() {
    let total: usize = (0..EXITS).sum();
    let plain = |s: &str| s.to_string();
    let cases = [
        (
            "?? return",
            "        let v{i} = at(xs, {i}) ?? return -1\n        s += v{i}\n",
            plain as fn(&str) -> String,
            format!("{total}\n-1"),
        ),
        (
            "let else",
            "        let v{i} = at(xs, {i}) else { return -1 }\n        s += v{i}\n",
            plain,
            format!("{total}\n-1"),
        ),
        (
            "if let else return",
            "        if let v{i} = at(xs, {i}) { s += v{i} } else { return -1 }\n",
            plain,
            format!("{total}\n-1"),
        ),
        (
            "?? continue",
            "        let v{i} = at(xs, {i}) ?? continue\n        s += v{i}\n",
            looped,
            format!("{}\n6", 2 * total),
        ),
        (
            "?? break",
            "        let v{i} = at(xs, {i}) ?? break\n        s += v{i}\n",
            looped,
            format!("{}\n3", 2 * total),
        ),
    ];
    for (name, step, wrap, expected) in cases {
        let src = chain(step, &wrap);
        assert_eq!(run_main(&emit_module(&src)), expected, "{name}");
    }
}
