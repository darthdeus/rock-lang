//! Rust-computed oracles for numbers, strings, matches, collections, closures
//! and coroutines. Inputs reach the program through vectors so the compiler
//! cannot fold them, and each check loops so the hot paths run as traces.

use super::generator::Generator;
use std::fmt::Write;

pub(super) fn areas(g: &mut Generator, out: &mut String) {
    out.push_str(PRELUDE);
    let calls = [
        integers(g, out),
        floats(g, out),
        strings(g, out),
        patterns(g, out),
        vectors(g, out),
        maps(g, out),
        closures(g, out),
        coroutines(g, out),
    ];
    writeln!(out, "fn check_areas() {{\n{}}}", calls.concat()).unwrap();
}

const PRELUDE: &str = r#"
fn same_float(got: float, want: float) -> bool {
    if is_nan(want) { return is_nan(got) }
    return got == want && (1.0 / got < 0.0) == (1.0 / want < 0.0)
}
fn show_float(x: float) -> string {
    if is_nan(x) { return "nan" }
    if x == 0.0 { return if 1.0 / x < 0.0 { "-zero" } else { "zero" } }
    return "{x} {to_string(x)} {x:.3}"
}
"#;

fn subset<T: Copy>(g: &mut Generator, pool: &[T], n: usize) -> Vec<T> {
    let mut items = pool.to_vec();
    for i in (1..items.len()).rev() {
        items.swap(i, g.below(i + 1));
    }
    items.truncate(n);
    items
}

fn list<T: std::fmt::Display>(items: impl IntoIterator<Item = T>) -> String {
    let items: Vec<String> = items.into_iter().map(|x| x.to_string()).collect();
    format!("[{}]", items.join(", "))
}

fn float_literal(x: f64) -> String {
    if x.is_nan() {
        "(zero / zero)".into()
    } else if x.is_infinite() {
        format!("({}1.0 / zero)", if x < 0.0 { "-" } else { "" })
    } else {
        format!("{x:?}")
    }
}

/// Rock `int` is a double on both backends: `+ - *` round like f64 and `./`
/// and `%` floor. Divisions stay where the double formula is exact.
fn integers(g: &mut Generator, out: &mut String) -> String {
    let pool: [i64; 16] = [
        0,
        1,
        -1,
        3,
        -7,
        46341,
        65535,
        2147483647,
        -2147483648,
        2147483648,
        4294967295,
        4294967296,
        94906265,
        -9007199254740991,
        9007199254740991,
        4503599627370496,
    ];
    let xs = subset(g, &pool, 7);
    let mut body = String::new();
    for op in ["+", "-", "*", "./", "%", "<", "==", "min", "max", "acc"] {
        let (mut lhs, mut rhs, mut want) = (Vec::new(), Vec::new(), Vec::new());
        for (i, &a) in xs.iter().enumerate() {
            for (j, &b) in xs.iter().enumerate() {
                let (fa, fb) = (a as f64, b as f64);
                let result = match op {
                    "+" => fa + fb,
                    "-" => fa - fb,
                    "*" => fa * fb,
                    "./" | "%" => {
                        if b == 0 || (a.abs() as f64) * 2.0 > 9007199254740991.0 {
                            continue;
                        }
                        let (q, r) = (a.div_euclid(b), a.rem_euclid(b));
                        let (q, r) = if r != 0 && b < 0 {
                            (q + 1, r + b)
                        } else {
                            (q, r)
                        };
                        let floor = (fa / fb).floor();
                        if floor != q as f64 || fa - floor * fb != r as f64 {
                            continue;
                        }
                        if op == "./" { q as f64 } else { r as f64 }
                    }
                    "<" => f64::from(u8::from(a < b)),
                    "==" => f64::from(u8::from(a == b)),
                    "min" => a.min(b) as f64,
                    "max" => a.max(b) as f64,
                    _ => fa + fb + 1.0,
                };
                if result.abs() > 9.2e18 {
                    continue;
                }
                lhs.push(i);
                rhs.push(j);
                want.push(result as i64);
            }
        }
        let expr = match op {
            "<" | "==" => format!("if xs[l[k]] {op} xs[r[k]] {{ 1 }} else {{ 0 }}"),
            "min" | "max" => format!("{op}(xs[l[k]], xs[r[k]])"),
            "acc" => "accumulate(xs[l[k]], xs[r[k]])".into(),
            _ => format!("xs[l[k]] {op} xs[r[k]]"),
        };
        writeln!(
            body,
            "let l{0}: [int] = {1}\nlet r{0}: [int] = {2}\nlet w{0}: [int] = {3}\nfor k in 0..l{0}.len() {{\nlet l = l{0}\nlet r = r{0}\nlet got = {expr}\nif got != w{0}[k] {{ print(\"int {op} {{xs[l[k]]}} {{xs[r[k]]}}: {{got}} != {{w{0}[k]}}\"); assert(false, \"int oracle\") }}\n}}",
            body.len(),
            list(&lhs),
            list(&rhs),
            list(&want)
        )
        .unwrap();
    }
    let sum: f64 = xs.iter().map(|&x| x as f64).sum();
    writeln!(
        out,
        "fn accumulate(a: int, b: int) -> int {{\nlet t = a\nt += b\nt += 1\nreturn t\n}}\nfn check_integers(xs: [int]) {{\n{body}let total = 0\nfor x in xs {{ total += x }}\nassert(total == {}, \"int sum oracle\")\n}}",
        sum as i64
    )
    .unwrap();
    format!("check_integers({})\n", list(&xs))
}

fn lua_mod(a: f64, b: f64) -> f64 {
    a - (a / b).floor() * b
}

fn floats(g: &mut Generator, out: &mut String) -> String {
    let pool = [
        0.0,
        -0.0,
        0.5,
        -1.5,
        3.0,
        -7.25,
        0.1,
        1e300,
        -1e300,
        1e-300,
        5e-324,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        9007199254740993.0,
        2147483647.5,
    ];
    let xs = subset(g, &pool, 7);
    let mut body = String::new();
    let ops: [(&str, fn(f64, f64) -> f64); 7] = [
        ("+", |a, b| a + b),
        ("-", |a, b| a - b),
        ("*", |a, b| a * b),
        ("/", |a, b| a / b),
        ("%", lua_mod),
        ("min", |a, b| if a < b { a } else { b }),
        ("max", |a, b| if a > b { a } else { b }),
    ];
    for (op, f) in ops {
        let mut want = Vec::new();
        for &a in &xs {
            for &b in &xs {
                want.push(float_literal(f(a, b)));
            }
        }
        let expr = if op.len() > 1 {
            format!("{op}(a, b)")
        } else {
            format!("a {op} b")
        };
        writeln!(
            body,
            "let w{0}: [float] = {1}\nfor i in 0..xs.len() {{\nfor j in 0..xs.len() {{\nlet a = xs[i]\nlet b = xs[j]\nlet got = {expr}\nif !same_float(got, w{0}[i * xs.len() + j]) {{ print(\"float {op} {{show_float(a)}} {{show_float(b)}}: {{show_float(got)}}\"); assert(false, \"float oracle\") }}\n}}\n}}",
            body.len(),
            list(want)
        )
        .unwrap();
    }
    let unary: [(&str, fn(f64) -> f64); 5] = [
        ("floor", f64::floor),
        ("abs", f64::abs),
        ("sqrt", f64::sqrt),
        ("round", |x| (x + 0.5).floor()),
        ("negate", |x| -x),
    ];
    for (name, f) in unary {
        let want: Vec<_> = xs.iter().map(|&x| float_literal(f(x))).collect();
        let call = if name == "negate" {
            "-xs[i]".into()
        } else {
            format!("{name}(xs[i])")
        };
        writeln!(
            body,
            "let w{0}: [float] = {1}\nfor i in 0..xs.len() {{ if !same_float({call}, w{0}[i]) {{ print(\"float {name} {{show_float(xs[i])}}\"); assert(false, \"float unary oracle\") }} }}",
            body.len(),
            list(want)
        )
        .unwrap();
    }
    // Formatting has no Rust oracle here; the backends and JIT modes must agree.
    let literals: Vec<_> = xs.iter().map(|&x| float_literal(x)).collect();
    writeln!(
        out,
        "fn check_floats(xs: [float], zero: float) {{\n{body}for x in xs {{ print(show_float(x)) }}\n}}"
    )
    .unwrap();
    format!(
        "let zero = [0.0][0]\ncheck_floats({}, zero)\n",
        list(literals)
    )
}

fn rock_string(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('{', "{{")
            .replace('}', "}}")
    )
}

fn strings(g: &mut Generator, out: &mut String) -> String {
    let pool = [
        "", "a", "abc", "a.b", "%d", "a-b-c", "[x]", "  pad ", "A_b", "x%y%", "..", "(a)", "a+b",
        "^$", "aaa", "b",
    ];
    let xs = subset(g, &pool, 6);
    let needles: Vec<&str> = xs.iter().copied().filter(|s| !s.is_empty()).collect();
    let quoted = |items: &[&str]| list(items.iter().map(|s| rock_string(s)));
    let mut want = Vec::new();
    for &s in &xs {
        want.push(s.len().to_string());
        want.push(s.to_uppercase());
        want.push(s.to_lowercase());
        want.push(s.trim().into());
        want.push(s.repeat(2));
        for &t in &xs {
            want.push(format!("{s}{t}"));
            want.push(s.contains(t).to_string());
            want.push(s.starts_with(t).to_string());
            want.push(s.ends_with(t).to_string());
            want.push(s.find(t).map_or(-1, |i| i as i64).to_string());
            want.push((s == t).to_string());
        }
        for &t in &needles {
            want.push(s.split(t).collect::<Vec<_>>().join("|"));
            want.push(s.replace(t, "<>"));
        }
        for start in 0..=s.len() {
            for end in start..=s.len() {
                want.push(s[start..end].into());
            }
        }
    }
    writeln!(
        out,
        "fn check_strings(xs: [string], needles: [string]) {{
let got: [string] = []
for s in xs {{
    got.push(\"{{s.len()}}\")
    got.push(s.upper())
    got.push(s.lower())
    got.push(s.trim())
    got.push(s.repeat(2))
    for t in xs {{
        got.push(s + t)
        got.push(\"{{s.contains(t)}}\")
        got.push(\"{{s.starts_with(t)}}\")
        got.push(\"{{s.ends_with(t)}}\")
        got.push(\"{{s.find(t)}}\")
        got.push(\"{{s == t}}\")
    }}
    for t in needles {{
        got.push(s.split(t).join(\"|\"))
        got.push(s.replace(t, \"<>\"))
    }}
    for start in 0..s.len() + 1 {{
        for end in start..s.len() + 1 {{ got.push(s.substring(start, end)) }}
    }}
}}
let want: [string] = {}
assert(got.len() == want.len(), \"string oracle count\")
for i in 0..want.len() {{
    if got[i] != want[i] {{ print(\"string oracle {{i}}: {{got[i]}} != {{want[i]}}\"); assert(false, \"string oracle\") }}
}}
for s in [\"12\", \" 12\", \"0x10\", \"1e3\", \"-0\", \"+5\", \"1.5\", \"abc\", \"\", \"9007199254740993\", \"08\", \"inf\", \"nan\", \"-.5\"] {{
    print(\"parse {{s}} {{label(s.parse_int() ?? -99)}} {{show_float(s.parse_float() ?? -99.0)}}\")
}}
}}",
        list(want.iter().map(|s| rock_string(s)))
    )
    .unwrap();
    format!("check_strings({}, {})\n", quoted(&xs), quoted(&needles))
}

#[derive(Clone, Copy, PartialEq)]
enum Inner {
    A(i64),
    B(i64, i64),
    C,
}

#[derive(Clone, Copy, PartialEq)]
enum Outer {
    Wrap(Inner),
    Pair(Inner, Inner),
    Opt(Option<i64>),
    Leaf,
}

#[derive(Clone, Copy)]
enum InnerPat {
    Any,
    A(Option<i64>),
    B(Option<i64>),
    C,
}

#[derive(Clone, Copy)]
enum Pat {
    Wrap(InnerPat),
    Pair(InnerPat, InnerPat),
    OptNull,
    OptSome(Option<i64>),
    Leaf,
}

impl Inner {
    fn rock(self) -> String {
        match self {
            Inner::A(n) => format!("Inner::A({n})"),
            Inner::B(a, b) => format!("Inner::B({a}, {b})"),
            Inner::C => "Inner::C".into(),
        }
    }
}

impl Outer {
    fn rock(self) -> String {
        match self {
            Outer::Wrap(i) => format!("Outer::Wrap({})", i.rock()),
            Outer::Pair(a, b) => format!("Outer::Pair({}, {})", a.rock(), b.rock()),
            Outer::Opt(Some(n)) => format!("Outer::Opt({n})"),
            Outer::Opt(None) => "Outer::Opt(null)".into(),
            Outer::Leaf => "Outer::Leaf".into(),
        }
    }
}

/// A pattern binds `n` at most once; `n` reads 0 where nothing binds it.
fn inner_pattern(p: InnerPat, bind: &mut bool) -> String {
    let mut name = || {
        if *bind {
            "_".to_string()
        } else {
            *bind = true;
            "n".to_string()
        }
    };
    match p {
        InnerPat::Any => "_".into(),
        InnerPat::A(Some(k)) => format!("Inner::A({k})"),
        InnerPat::A(None) => format!("Inner::A({})", name()),
        InnerPat::B(Some(k)) => format!("Inner::B({k}, _)"),
        InnerPat::B(None) => format!("Inner::B(_, {})", name()),
        InnerPat::C => "Inner::C".into(),
    }
}

fn inner_matches(p: InnerPat, v: Inner, bound: &mut Option<i64>) -> bool {
    match (p, v) {
        (InnerPat::Any, _) | (InnerPat::C, Inner::C) => true,
        (InnerPat::A(Some(k)), Inner::A(n)) | (InnerPat::B(Some(k)), Inner::B(n, _)) => k == n,
        (InnerPat::A(None), Inner::A(n)) | (InnerPat::B(None), Inner::B(_, n)) => {
            bound.get_or_insert(n);
            true
        }
        _ => false,
    }
}

fn patterns(g: &mut Generator, out: &mut String) -> String {
    let ints = [-2, 0, 1, 5];
    let inner = |g: &mut Generator| match g.below(3) {
        0 => Inner::A(g.pick(&ints)),
        1 => Inner::B(g.pick(&ints), g.pick(&ints)),
        _ => Inner::C,
    };
    let values: Vec<Outer> = (0..10)
        .map(|_| match g.below(5) {
            0 => Outer::Wrap(inner(g)),
            1 => Outer::Pair(inner(g), inner(g)),
            2 => Outer::Opt(Some(g.pick(&ints))),
            3 => Outer::Opt(None),
            _ => Outer::Leaf,
        })
        .collect();
    let inner_pat = |g: &mut Generator| match g.below(6) {
        0 => InnerPat::Any,
        1 => InnerPat::A(Some(g.pick(&ints))),
        2 => InnerPat::A(None),
        3 => InnerPat::B(Some(g.pick(&ints))),
        4 => InnerPat::B(None),
        _ => InnerPat::C,
    };
    let arms: Vec<(Pat, Option<i64>)> = (0..g.pick(&[2, 4, 6]))
        .map(|_| {
            let pat = match g.below(5) {
                0 => Pat::Wrap(inner_pat(g)),
                1 => Pat::Pair(inner_pat(g), inner_pat(g)),
                2 => Pat::OptNull,
                3 => Pat::OptSome(g.pick(&[None, Some(1), Some(5)])),
                _ => Pat::Leaf,
            };
            (pat, g.pick(&[None, Some(0), Some(2)]))
        })
        .collect();
    let mut source = String::from("fn classify(o: Outer) -> int {\nreturn match o {\n");
    for (index, (pat, guard)) in arms.iter().enumerate() {
        let mut bind = false;
        let text = match *pat {
            Pat::Wrap(p) => format!("Outer::Wrap({})", inner_pattern(p, &mut bind)),
            Pat::Pair(a, b) => format!(
                "Outer::Pair({}, {})",
                inner_pattern(a, &mut bind),
                inner_pattern(b, &mut bind)
            ),
            Pat::OptNull => "Outer::Opt(null)".into(),
            Pat::OptSome(Some(k)) => format!("Outer::Opt({k})"),
            Pat::OptSome(None) => {
                bind = true;
                "Outer::Opt(n)".into()
            }
            Pat::Leaf => "Outer::Leaf".into(),
        };
        let n = if bind {
            if matches!(pat, Pat::OptSome(None)) {
                "(n ?? -50)"
            } else {
                "n"
            }
        } else {
            "0"
        };
        let guard = guard.map_or(String::new(), |k| format!(" if {n} > {k}"));
        writeln!(source, "{text}{guard} => {n} * 10 + {index},").unwrap();
    }
    source.push_str("_ => -1,\n}\n}\n");
    let want: Vec<i64> = values
        .iter()
        .map(|&v| {
            for (index, &(pat, guard)) in arms.iter().enumerate() {
                let mut bound = None;
                let matched = match (pat, v) {
                    (Pat::Wrap(p), Outer::Wrap(i)) => inner_matches(p, i, &mut bound),
                    (Pat::Pair(p, q), Outer::Pair(a, b)) => {
                        inner_matches(p, a, &mut bound) && inner_matches(q, b, &mut bound)
                    }
                    (Pat::OptNull, Outer::Opt(None)) => true,
                    (Pat::OptSome(Some(k)), Outer::Opt(Some(n))) => k == n,
                    (Pat::OptSome(None), Outer::Opt(o)) => {
                        bound = Some(o.unwrap_or(-50));
                        true
                    }
                    (Pat::Leaf, Outer::Leaf) => true,
                    _ => false,
                };
                let n = bound.unwrap_or(0);
                if matched && guard.is_none_or(|k| n > k) {
                    return n * 10 + index as i64;
                }
            }
            -1
        })
        .collect();
    out.push_str("enum Inner { A(int), B(int, int), C }\nenum Outer { Wrap(Inner), Pair(Inner, Inner), Opt(int?), Leaf }\n");
    out.push_str(&source);
    writeln!(
        out,
        "fn check_patterns(values: [Outer], want: [int]) {{\nfor i in 0..values.len() {{\nlet got = classify(values[i])\nif got != want[i] {{ print(\"match {{i}}: {{got}} != {{want[i]}}\"); assert(false, \"match oracle\") }}\n}}\n}}"
    )
    .unwrap();
    format!(
        "check_patterns({}, {})\n",
        list(values.iter().map(|v| v.rock())),
        list(&want)
    )
}

fn vectors(g: &mut Generator, out: &mut String) -> String {
    let mut model: Vec<i64> = vec![3, 1, 4];
    let mut ops = String::new();
    for _ in 0..12 {
        let len = model.len() as i64;
        let index = g.pick(&[0, 1, 2, -1, 5]);
        match g.below(8) {
            0 | 1 => {
                let x = g.pick(&[-1, 0, 7, -2147483648]);
                model.push(x);
                writeln!(ops, "v.push({x})\nrows.push(V {{ x: {x}, y: 1 }})").unwrap();
            }
            2 => {
                let popped = model.pop().unwrap_or(-99);
                writeln!(ops, "assert((v.pop() ?? -99) == {popped}, \"pop oracle\")\nassert((rows.pop()?.x ?? -99) == {popped}, \"row pop oracle\")").unwrap();
            }
            3 => {
                let removed = if (0..len).contains(&index) {
                    model.remove(index as usize)
                } else {
                    -99
                };
                writeln!(ops, "assert((v.remove_at({index}) ?? -99) == {removed}, \"remove_at oracle\")\nassert((rows.remove_at({index})?.x ?? -99) == {removed}, \"row remove_at oracle\")").unwrap();
            }
            4 => {
                let removed = if (0..len).contains(&index) {
                    model.swap_remove(index as usize)
                } else {
                    -99
                };
                writeln!(ops, "assert((v.swap_remove({index}) ?? -99) == {removed}, \"swap_remove oracle\")\nassert((rows.swap_remove({index})?.x ?? -99) == {removed}, \"row swap_remove oracle\")").unwrap();
            }
            5 => {
                model.sort_unstable_by_key(|x| std::cmp::Reverse(*x));
                ops.push_str("v.sort(x => -x)\nrows.sort(r => -r.x)\n");
            }
            6 => {
                model.reverse();
                ops.push_str("v = v.rev()\nrows = rows.rev()\n");
            }
            _ if (0..len).contains(&index) => {
                model[index as usize] += 5;
                writeln!(ops, "v[{index}] += 5\nrows[{index}].x += 5").unwrap();
            }
            _ => {
                let got = model
                    .get(index as usize)
                    .filter(|_| index >= 0)
                    .copied()
                    .unwrap_or(-99);
                writeln!(ops, "assert((v.get({index}) ?? -99) == {got}, \"get oracle\")\nassert((rows.get({index})?.x ?? -99) == {got}, \"row get oracle\")").unwrap();
            }
        }
        writeln!(
            ops,
            "assert(v.len() == {0} && rows.len() == {0}, \"len oracle\")",
            model.len()
        )
        .unwrap();
    }
    let sum: i64 = model.iter().sum();
    writeln!(
        out,
        "fn check_vectors(seed: [int]) {{
let v: [int] = []
let rows: [V] = []
for x in seed {{ v.push(x)\nrows.push(V {{ x: x, y: 1 }}) }}
{ops}let total = 0
for x in v {{ total += x }}
for r in rows {{ total -= r.x }}
assert(total == 0, \"rows mirror ints\")
for i in 0..v.len() {{ total += v[i] }}
assert(total == {sum}, \"vector sum oracle\")
for x in v {{ if x % 2 == 0 {{ v.push(x + 1) }} }}
for r in rows {{ if r.x % 2 == 0 {{ rows.push(V {{ x: r.x + 1, y: 2 }}) }} }}
print(\"grown {{v.len()}} {{rows.len()}}\")
}}"
    )
    .unwrap();
    "check_vectors([3, 1, 4])\n".into()
}

fn maps(g: &mut Generator, out: &mut String) -> String {
    let mut model = std::collections::BTreeMap::new();
    let mut ops = String::new();
    for _ in 0..14 {
        let key = g.pick(&["a", "b", "__proto__", "constructor", "", "toString"]);
        match g.below(4) {
            0 | 1 => {
                let value = g.pick(&[0_i64, 1, -5, 4294967296]);
                model.insert(key, value);
                writeln!(ops, "m[\"{key}\"] = {value}").unwrap();
            }
            2 => {
                let removed = model.remove(key).unwrap_or(-99);
                writeln!(
                    ops,
                    "assert((m.remove(\"{key}\") ?? -99) == {removed}, \"map remove oracle\")"
                )
                .unwrap();
            }
            _ => {
                let got = model.get(key).copied().unwrap_or(-99);
                writeln!(ops, "assert((m.get(\"{key}\") ?? -99) == {got} && m.contains(\"{key}\") == {}, \"map get oracle\")", model.contains_key(key)).unwrap();
            }
        }
        writeln!(
            ops,
            "assert(m.len() == {}, \"map len oracle\")",
            model.len()
        )
        .unwrap();
    }
    let keys: Vec<String> = model.keys().map(|k| rock_string(k)).collect();
    writeln!(
        out,
        "fn check_maps() {{
let m: Map<string, int> = map_new()
{ops}let keys = m.keys()
let want: [string] = {}
assert(keys.len() == want.len(), \"map keys count\")
for i in 0..keys.len() {{ assert(keys[i] == want[i], \"map keys oracle\") }}
for k in m.keys() {{ if m[k] > 0 {{ m.remove(k) }} else {{ m[k + \"!\"] = 1 }} }}
print(\"map after {{m.len()}}\")
}}",
        list(keys)
    )
    .unwrap();
    "check_maps()\n".into()
}

fn closures(g: &mut Generator, out: &mut String) -> String {
    let n = g.pick(&[1, 2, 4]);
    let step = g.pick(&[-3, 1, 7]);
    let (start, late) = (g.pick(&[0, 5]), g.pick(&[100, -100]));
    let calls: Vec<usize> = (0..6).map(|_| g.below(n)).collect();
    let mut shared = late;
    let mut fresh = vec![0i64; n];
    let mut want = Vec::new();
    for &c in &calls {
        shared += step;
        fresh[c] += 1;
        want.push(shared * 1000 + c as i64 * 10 + fresh[c]);
    }
    writeln!(
        out,
        "fn make_counter(base: int) -> fn() -> int {{
let count = base
return () => {{ count += 1; return count }}
}}
fn check_closures(calls: [int]) {{
let shared = {start}
let fs: [fn() -> int] = []
let i = 0
while i < {n} {{
    let index = i
    let mine = make_counter(0)
    fs.push(() => {{ shared += {step}; return shared * 1000 + index * 10 + mine() }})
    i += 1
}}
shared = {late}
let want: [int] = {}
for k in 0..calls.len() {{
    let got = fs[calls[k]]()
    if got != want[k] {{ print(\"closure {{k}}: {{got}} != {{want[k]}}\"); assert(false, \"closure oracle\") }}
}}
let nested = 0
let outer = (x: int) => {{
    let inner = (y: int) => {{ nested += x * y; return nested }}
    return inner(x) + inner(1)
}}
assert(outer(3) == 21 && nested == 12, \"nested capture oracle\")
}}",
        list(&want)
    )
    .unwrap();
    format!("check_closures({})\n", list(&calls))
}

fn coroutines(g: &mut Generator, out: &mut String) -> String {
    let depth = g.pick(&[1, 2, 3]);
    let counts: Vec<i64> = (0..depth).map(|_| g.pick(&[0, 1, 2])).collect();
    let kill_after = g.pick(&[None, Some(0), Some(1), Some(3)]);
    let mut source = String::from("global CO_LOG: [string] = []\n");
    // Level 0 is the leaf; level k awaits level k - 1 and then yields its result.
    let mut yields = Vec::new();
    let mut result = 0;
    for (level, &count) in counts.iter().enumerate() {
        let delegate = if level == 0 {
            String::new()
        } else {
            format!(
                "let r = await level{}(n + 1)\nyield r\ntotal += r\n",
                level - 1
            )
        };
        writeln!(
            source,
            "co fn level{level}(n: int) -> Coroutine<int, int, ()> {{
defer {{ CO_LOG.push(\"exit{level}\") }}
let total = n
{delegate}for i in 0..{count} {{
    defer {{ CO_LOG.push(\"step{level}\") }}
    yield n * 100 + i
    total += i
}}
return total
}}"
        )
        .unwrap();
    }
    // Model: level k with argument n yields its delegate's stream, then the
    // delegate's result, then its own steps.
    fn stream(level: usize, n: i64, counts: &[i64], yields: &mut Vec<i64>) -> i64 {
        let mut total = n;
        if level > 0 {
            let r = stream(level - 1, n + 1, counts, yields);
            yields.push(r);
            total += r;
        }
        for i in 0..counts[level] {
            yields.push(n * 100 + i);
            total += i;
        }
        total
    }
    result += stream(depth - 1, 1, &counts, &mut yields);
    let taken = kill_after.map_or(yields.len(), |k| k.min(yields.len()));
    let finished = taken == yields.len();
    out.push_str(&source);
    writeln!(
        out,
        "fn check_coroutines(want: [int]) {{
CO_LOG = []
let c = level{}(1)
let got: [int] = []
while got.len() < {taken} {{
    if let y = c.resume() {{ got.push(y) }} else {{ break }}
}}
{}
assert(got.len() == want.len(), \"coroutine yield count\")
for i in 0..want.len() {{ assert(got[i] == want[i], \"coroutine yield oracle\") }}
print(\"coroutine log {{CO_LOG.join(\",\")}} {{CO_LOG.len()}}\")
}}",
        depth - 1,
        if finished {
            format!(
                "assert(c.resume() == null && (c.result() ?? -1) == {result}, \"coroutine result oracle\")"
            )
        } else {
            "kill(c)\nassert(c.resume() == null, \"killed coroutine stays dead\")".into()
        }
    )
    .unwrap();
    format!("check_coroutines({})\n", list(&yields[..taken]))
}
