use super::generator::Generator;
use std::fmt::Write;

pub(super) fn loops(g: &mut Generator, out: &mut String) {
    out.push_str("global control: int = 0\nfn write_control(value: int) { control = value }\n");
    for (name, setup, counter, bound, write) in [
        ("local_bound", "let n = limit", "i", "n", "n = changed"),
        (
            "closure_bound",
            "let n = limit\nlet update = (value: int) => { n = value }",
            "i",
            "n",
            "update(changed)",
        ),
        (
            "callee_bound",
            "control = limit",
            "i",
            "control",
            "write_control(changed)",
        ),
        ("local_counter", "", "i", "limit", "i = changed"),
        (
            "closure_counter",
            "let update = (value: int) => { i = value }",
            "i",
            "limit",
            "update(changed)",
        ),
        (
            "callee_counter",
            "control = 0",
            "control",
            "limit",
            "write_control(changed)",
        ),
    ] {
        writeln!(
            out,
            "fn {name}(limit: int, trigger: int, changed: int) -> int {{
let i = 0
{setup}
let trace = 0
while {counter} < {bound} {{
    trace = trace * 7 + {counter} + 1
    if {counter} == trigger {{ {write} }}
    {counter} += 1
}}
return trace
}}"
        )
        .unwrap();
    }
    out.push_str(
        r#"
fn reentered(w: int, h: int, stop: int, skip: int) -> int {
    let col = 0
    let trace = 0
    for row in 0..h {
        while col < w * h {
            if col == stop { break }
            if col == skip {
                col += 1
                continue
            }
            trace = trace * 7 + row + col + 1
            col += 1
        }
    }
    return trace
}
fn unit_step(n: int) -> int {
    let x = 0
    while x < n {
        x = x + 1
    }
    return x
}
fn latch_work(n: int, stop: int) -> int {
    let i = 0
    let trace = 0
    while i < n {
        if i == stop { break }
        trace = trace * 7 + i + 1
        i += 1
    }
    return trace
}
fn continue_counter(limit: int, trigger: int, changed: int) -> int {
    let i = 0
    let trace = 0
    while i < limit {
        if i == trigger {
            i = changed
            continue
        }
        trace = trace * 7 + i + 1
        i += 1
    }
    return trace
}
fn check_generated_loops() {
"#,
    );
    for limit in [0, g.pick(&[4, 5, 6]), g.pick(&[5, 6, 7])] {
        let trigger = g.pick(&[0, 1]);
        let changed = trigger + g.pick(&[2, 3]);
        for kind in ["bound", "counter"] {
            let (mut n, mut i, mut trace) = (limit, 0, 0);
            while i < n {
                trace = trace * 7 + i + 1;
                if i == trigger {
                    if kind == "bound" {
                        n = changed;
                    } else {
                        i = changed;
                    }
                }
                i += 1;
            }
            for writer in ["local", "closure", "callee"] {
                writeln!(out, "assert({writer}_{kind}({limit}, {trigger}, {changed}) == {trace}, \"{writer}_{kind} oracle\")").unwrap();
            }
        }
        let (mut i, mut trace) = (0, 0);
        while i < limit {
            if i == trigger {
                i = changed;
                continue;
            }
            trace = trace * 7 + i + 1;
            i += 1;
        }
        writeln!(out, "assert(continue_counter({limit}, {trigger}, {changed}) == {trace}, \"continue_counter oracle\")").unwrap();
        let mut trace = 0;
        for i in 0..limit.min(changed) {
            trace = trace * 7 + i + 1;
        }
        writeln!(out, "assert(unit_step({limit}) == {limit}, \"unit_step oracle\")\nassert(latch_work({limit}, {changed}) == {trace}, \"latch_work oracle\")").unwrap();
    }
    let (w, h) = (g.pick(&[1, 2]), g.pick(&[2, 3]));
    let skip = g.pick(&[0, 1]);
    for stop in [w * h, g.pick(&[2, 3])] {
        let mut trace = 0;
        for col in (0..(w * h).min(stop)).filter(|&col| col != skip) {
            trace = trace * 7 + col + 1;
        }
        writeln!(
            out,
            "assert(reentered({w}, {h}, {stop}, {skip}) == {trace}, \"reentered oracle\")"
        )
        .unwrap();
    }
    out.push_str("}\n");
}

enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(&'static str),
    Str(&'static str),
    Array(Vec<Json>),
    Object(Vec<(&'static str, Json)>),
}

impl Json {
    fn rock(&self) -> String {
        match self {
            Self::Null => "json_null()".into(),
            Self::Bool(b) => format!("json_bool({b})"),
            Self::Int(n) => format!("json_int({n})"),
            Self::Float(n) => format!("json_float({n})"),
            Self::Str(s) => format!("json_str({})", rock_string(s)),
            Self::Array(items) => {
                let items: Vec<_> = items.iter().map(Self::rock).collect();
                format!("json_array([{}])", items.join(", "))
            }
            Self::Object(entries) => {
                let pairs: Vec<_> = entries
                    .iter()
                    .map(|(key, value)| format!("({}, {})", rock_string(key), value.rock()))
                    .collect();
                format!("json_object(map_of([{}]))", pairs.join(", "))
            }
        }
    }

    fn wire(&self) -> String {
        let quote = |s| serde_json::to_string(s).unwrap();
        match self {
            Self::Null => "null".into(),
            Self::Bool(b) => b.to_string(),
            Self::Int(n) => n.to_string(),
            Self::Float(n) => (*n).into(),
            Self::Str(s) => quote(s),
            Self::Array(items) => {
                let items: Vec<_> = items.iter().map(Self::wire).collect();
                format!("[{}]", items.join(","))
            }
            Self::Object(entries) => {
                let fields: Vec<_> = entries
                    .iter()
                    .map(|(key, value)| format!("{}:{}", quote(key), value.wire()))
                    .collect();
                format!("{{{}}}", fields.join(","))
            }
        }
    }
}

fn rock_string(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '{' => out.push_str("{{"),
            '}' => out.push_str("}}"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ' '..='~' => out.push(ch),
            _ => write!(out, "\\u{{{:x}}}", u32::from(ch)).unwrap(),
        }
    }
    out.push('"');
    out
}

pub(super) fn json(g: &mut Generator, out: &mut String) {
    let mut items = vec![
        Json::Null,
        Json::Bool(g.pick(&[true, false])),
        Json::Int(g.pick(&[
            -9007199254740992,
            -9007199254740991,
            9007199254740991,
            9007199254740992,
        ])),
        Json::Float("0.1"),
        Json::Float("1e300"),
        Json::Float("-0.0"),
        Json::Float(g.pick(&["5e-324", "1e-320", "-5e-324"])),
        Json::Str(g.pick(&["a\n\t\\\"\u{1}", "\u{1f600}\u{e9}", "{{\u{2028}}}"])),
    ];
    for i in (1..items.len()).rev() {
        let j = g.pick(&(0..=i).collect::<Vec<_>>());
        items.swap(i, j);
    }
    let mut tree = Json::Array(items);
    for _ in 0..g.pick(&[2, 3, 4]) {
        let key = g.pick(&["a", "__proto__", "q\\\"\u{1f680}"]);
        tree = Json::Object(vec![
            (key, tree),
            ("other", Json::Array(vec![Json::Bool(false), Json::Null])),
        ]);
        if g.pick(&[true, false]) {
            tree = Json::Array(vec![tree, Json::Int(g.pick(&[-1, 0, 1]))]);
        }
    }
    out.push_str(
        r#"
fn map_of(entries: [(string, JsonValue)]) -> Map<string, JsonValue> {
    let result: Map<string, JsonValue> = map_new()
    for (key, value) in entries { result[key] = value }
    return result
}

fn check_json_value(want: JsonValue, got: JsonValue) {
    match want {
        Null => assert(json_is_null(got), "JSON null oracle"),
        True => assert(json_as_bool(got).unwrap(), "JSON true oracle"),
        False => assert(!json_as_bool(got).unwrap(), "JSON false oracle"),
        Int(value) => assert(json_as_int(got).unwrap() == value, "JSON int oracle"),
        Float(value) => {
            let actual = json_as_float(got).unwrap()
            assert(actual == value && (1.0 / actual < 0.0) == (1.0 / value < 0.0), "JSON float oracle")
        },
        Str(value) => assert(json_as_str(got).unwrap() == value, "JSON string oracle"),
        Array(items) => {
            let actual = json_as_array(got).unwrap()
            assert(actual.len() == items.len(), "JSON array length oracle")
            for i in 0..items.len() { check_json_value(items[i], actual[i]) }
        },
        Object(fields) => {
            let actual = json_as_object(got).unwrap()
            assert(actual.len() == fields.len(), "JSON object length oracle")
            for key in fields.keys() { check_json_value(fields[key], actual[key]) }
        },
    }
}
"#,
    );
    writeln!(
        out,
        "fn check_generated_json() {{
let value = {}
check_json_value(value, json_decode({}).unwrap())
check_json_value(value, json_decode(json_encode(value)).unwrap())
check_json_value(value, json_decode(json_encode_pretty(value)).unwrap())
}}
",
        tree.rock(),
        rock_string(&tree.wire())
    )
    .unwrap();
}
