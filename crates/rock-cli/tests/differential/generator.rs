use std::fmt::Write;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ty {
    Int,
    Uint,
    Float,
    Bool,
    String,
    Value,
    Flat,
    Cell,
    Nested,
    MaybeInt,
    MaybeValue,
    MaybeCell,
    Vec,
    Rows,
    NestedRows,
    Map,
    Closure,
    Functions,
    Event,
    RefValue,
    RefFlat,
}

impl Ty {
    fn observer(self) -> &'static str {
        match self {
            Self::MaybeInt | Self::MaybeValue | Self::MaybeCell => "observe_optional",
            _ => "observe",
        }
    }

    fn rock(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Uint => "uint",
            Self::Float => "float",
            Self::Bool => "bool",
            Self::String => "string",
            Self::Value => "V",
            Self::Flat => "N",
            Self::Cell => "Cell",
            Self::Nested => "Nested",
            Self::MaybeInt => "int?",
            Self::MaybeValue => "V?",
            Self::MaybeCell => "Cell?",
            Self::Vec => "[int]",
            Self::Rows => "[V]",
            Self::NestedRows => "[Nested]",
            Self::Map => "Map<string, V>",
            Self::Closure => "fn() -> int",
            Self::Functions => "[fn() -> int]",
            Self::Event => "Event",
            Self::RefValue => "ref V",
            Self::RefFlat => "ref N",
        }
    }
}

#[derive(Clone)]
struct Variable {
    name: String,
    ty: Ty,
    counter: bool,
}

type Env = Vec<Variable>;

pub(super) struct Generator {
    random: u64,
    serial: usize,
    helpers: Vec<(String, Ty)>,
    overloads: bool,
    variants: Vec<(String, Ty)>,
}

impl Generator {
    pub(super) fn pick<T: Copy>(&mut self, choices: &[T]) -> T {
        self.random = self.random.wrapping_add(0x9e3779b97f4a7c15);
        let mut n = self.random;
        n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
        n ^= n >> 31;
        choices[(n % choices.len() as u64) as usize]
    }

    pub(super) fn below(&mut self, n: usize) -> usize {
        self.pick(&(0..n).collect::<Vec<_>>())
    }

    fn fresh(&mut self) -> String {
        self.serial += 1;
        format!("v{}", self.serial)
    }

    fn variable(&mut self, env: &Env, ty: Ty) -> Option<Variable> {
        let choices: Vec<_> = env.iter().filter(|v| v.ty == ty).collect();
        if choices.is_empty() {
            None
        } else {
            Some(self.pick(&choices).clone())
        }
    }

    fn expr(&mut self, env: &Env, ty: Ty, depth: usize) -> String {
        if self.pick(&[true, false]) {
            if let Some(v) = self.variable(env, ty) {
                return v.name;
            }
        }
        if depth > 0 {
            let action = self.pick(&[0, 1, 2, 3, 4]);
            if action == 0 {
                let calls: Vec<_> = self
                    .helpers
                    .iter()
                    .filter(|(_, t)| *t == ty)
                    .map(|(n, _)| n.clone())
                    .collect();
                if !calls.is_empty() {
                    let name = self.pick(&(0..calls.len()).collect::<Vec<_>>());
                    return format!("{}({})", calls[name], self.expr(env, Ty::Int, depth - 1));
                }
            }
            if action == 1
                && matches!(
                    ty,
                    Ty::Int
                        | Ty::Uint
                        | Ty::Float
                        | Ty::Value
                        | Ty::Flat
                        | Ty::Nested
                        | Ty::MaybeInt
                        | Ty::MaybeValue
                        | Ty::MaybeCell
                )
            {
                let value = self.expr(env, ty, depth - 1);
                let flag = self.expr(env, Ty::Bool, depth - 1);
                return format!("generic<{}>({value}, {flag})", ty.rock());
            }
            match ty {
                Ty::Int | Ty::Uint | Ty::Float => {
                    if self.overloads && action == 2 && ty == Ty::Int {
                        let arg = self.pick(&[Ty::Int, Ty::Value, Ty::Flat]);
                        return format!("over({})", self.expr(env, arg, depth - 1));
                    }
                    let left = self.expr(env, ty, depth - 1);
                    let right = self.expr(env, ty, depth - 1);
                    let op = self.pick(if ty == Ty::Float {
                        &["+", "-", "*", "/", "%"][..]
                    } else {
                        &["+", "-", "*", "./", "%", "&", "|", "^", "<<", ">>"][..]
                    });
                    return match ty {
                        // Each integer node stays exact and below LuaJIT's portable bitwise range.
                        Ty::Int => match op {
                            "<<" | ">>" => format!(
                                "(({left} & 2147483647) {op} {})",
                                self.pick(&[0, 1, 15, 31])
                            ),
                            "./" | "%" => format!("({left} {op} {})", self.pick(&[1, 2, -3, 31])),
                            "*" => format!("(({left} % 10000) * ({right} % 10000))"),
                            _ => format!("(({left} % 100000) {op} ({right} % 100000))"),
                        },
                        Ty::Uint => {
                            if matches!(op, "<<" | ">>") {
                                format!("({left} {op} {})", self.pick(&[0, 1, 15, 31]))
                            } else if matches!(op, "./" | "%") {
                                format!("({left} {op} ({} as uint))", self.pick(&[1, 2, 31]))
                            } else {
                                format!("({left} {op} {right})")
                            }
                        }
                        _ => {
                            if matches!(op, "/" | "%") {
                                format!("({left} {op} {})", self.pick(&["0.5", "2.0", "-3.0"]))
                            } else {
                                format!("(({left} % 1024.0) {op} ({right} % 1024.0))")
                            }
                        }
                    };
                }
                Ty::Bool => {
                    let a = self.expr(env, Ty::Int, depth - 1);
                    let b = self.expr(env, Ty::Int, depth - 1);
                    return format!("({a} {} {b})", self.pick(&["==", "!=", "<", ">="]));
                }
                Ty::String => {
                    let t = self.pick(&[Ty::Int, Ty::Bool, Ty::Uint]);
                    return format!("\"text-{{label({})}}\"", self.expr(env, t, depth - 1));
                }
                _ => {}
            }
        }
        match ty {
            Ty::Int => {
                let fields: Vec<_> = env
                    .iter()
                    .filter_map(|v| match v.ty {
                        Ty::Value | Ty::Flat | Ty::RefValue | Ty::RefFlat => Some(format!(
                            "{}.{}",
                            v.name,
                            if self.pick(&[true, false]) { "x" } else { "y" }
                        )),
                        Ty::Cell | Ty::Nested => Some(format!("{}.n.x", v.name)),
                        Ty::MaybeInt => Some(format!("({} ?? 0)", v.name)),
                        Ty::MaybeValue => Some(format!("({}?.x ?? 0)", v.name)),
                        Ty::MaybeCell => Some(format!("({}?.v?.x ?? 0)", v.name)),
                        Ty::Vec => Some(format!("{}[0]", v.name)),
                        Ty::Rows => Some(format!("{}[0].x", v.name)),
                        Ty::NestedRows => Some(format!("{}[0].v.x", v.name)),
                        _ => None,
                    })
                    .collect();
                if !fields.is_empty() && self.pick(&[true, false]) {
                    let index = self.pick(&(0..fields.len()).collect::<Vec<_>>());
                    fields[index].clone()
                } else {
                    // Value-struct int fields are signed 32-bit by definition.
                    self.pick(&["0", "1", "-1", "31", "2147483647", "-2147483648"])
                        .into()
                }
            }
            Ty::Uint => format!(
                "({} as uint)",
                self.pick(&[0_u64, 1, 31, 2147483648, 4294967295])
            ),
            Ty::Float => self
                .pick(&[
                    "0.0",
                    "-0.0",
                    "0.5",
                    "-1.25",
                    "0.000001",
                    "1024.0",
                    "9007199254740991.0",
                    "0.0000000000000002220446049250313",
                    "2.2250738585072014e-308",
                    "5e-324",
                ])
                .into(),
            Ty::Bool => self.pick(&["true", "false"]).into(),
            Ty::String => self
                .pick(&["\"\"", "\"a\"", "\"__proto__\"", "\"value\""])
                .into(),
            Ty::Value | Ty::Flat => {
                let x = self.expr(env, Ty::Int, 0);
                let y = self.expr(env, Ty::Int, 0);
                format!("{} {{ x: {x}, y: {y} }}", ty.rock())
            }
            Ty::Cell | Ty::Nested => format!(
                "{} {{ v: {}, n: {} }}",
                ty.rock(),
                self.expr(env, Ty::Value, 0),
                self.expr(env, Ty::Flat, 0)
            ),
            Ty::MaybeInt | Ty::MaybeValue | Ty::MaybeCell => {
                if self.pick(&[true, false]) {
                    "null".into()
                } else {
                    self.expr(
                        env,
                        match ty {
                            Ty::MaybeInt => Ty::Int,
                            Ty::MaybeValue => Ty::Value,
                            _ => Ty::Cell,
                        },
                        depth,
                    )
                }
            }
            Ty::Vec => format!("[{}]", self.expr(env, Ty::Int, 0)),
            Ty::Rows => format!("[{}]", self.expr(env, Ty::Value, 0)),
            Ty::NestedRows => format!("[{}]", self.expr(env, Ty::Nested, 0)),
            Ty::Map => "map_new()".into(),
            Ty::Closure => {
                let captures: Env = env
                    .iter()
                    .filter(|v| !matches!(v.ty, Ty::RefValue | Ty::RefFlat))
                    .cloned()
                    .collect();
                let x = self.expr(&captures, Ty::Int, 0);
                format!("() => {x}")
            }
            Ty::Functions => "[]".into(),
            Ty::Event => {
                let index = self.pick(&(0..=self.variants.len()).collect::<Vec<_>>());
                if let Some((name, payload)) = self.variants.get(index).cloned() {
                    format!("Event::{name}({})", self.expr(env, payload, 0))
                } else {
                    "Event::Empty".into()
                }
            }
            Ty::RefValue | Ty::RefFlat => unreachable!(),
        }
    }

    fn trace(&mut self, out: &mut String, value: &str) {
        self.serial += 1;
        writeln!(out, "observe({}, {value})", self.serial).unwrap();
    }

    fn trace_typed(&mut self, out: &mut String, value: &str, ty: Ty) {
        self.serial += 1;
        writeln!(out, "{}({}, {value})", ty.observer(), self.serial).unwrap();
    }

    fn trace_variable(&mut self, out: &mut String, v: &Variable) {
        if matches!(v.ty, Ty::RefValue | Ty::RefFlat) {
            self.trace(out, &format!("{}.x", v.name));
            self.trace(out, &format!("{}.y", v.name));
        } else {
            self.trace_typed(out, &v.name, v.ty);
        }
    }

    fn bind(&mut self, out: &mut String, env: &mut Env, ty: Ty) {
        let name = self.fresh();
        let value = self.expr(env, ty, 2);
        writeln!(out, "let {name}: {} = {value}", ty.rock()).unwrap();
        self.trace_typed(out, &name, ty);
        env.push(Variable {
            name,
            ty,
            counter: false,
        });
    }

    fn body(
        &mut self,
        out: &mut String,
        env: &mut Env,
        budget: &mut usize,
        depth: usize,
        in_loop: bool,
        coroutine: bool,
    ) {
        let count = self.pick(&[3, 4, 5]);
        for _ in 0..count {
            if *budget == 0 {
                break;
            }
            *budget -= 1;
            let action = self.pick(&[0, 0, 1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
            match action {
                0 => {
                    let ty = self.pick(&[
                        Ty::Int,
                        Ty::Nested,
                        Ty::MaybeInt,
                        Ty::MaybeValue,
                        Ty::MaybeCell,
                        Ty::Uint,
                        Ty::Float,
                        Ty::Bool,
                        Ty::String,
                        Ty::Value,
                        Ty::Flat,
                        Ty::Cell,
                        Ty::Vec,
                        Ty::Rows,
                        Ty::NestedRows,
                        Ty::Map,
                        Ty::Closure,
                        Ty::Functions,
                        Ty::Event,
                    ]);
                    self.bind(out, env, ty);
                }
                1 | 6 if !env.is_empty() => {
                    let v = self.pick(&env.iter().collect::<Vec<_>>()).clone();
                    if v.counter {
                        continue;
                    }
                    let (place, ty) = match v.ty {
                        Ty::Value | Ty::Flat | Ty::RefValue | Ty::RefFlat => {
                            (format!("{}.{}", v.name, self.pick(&["x", "y"])), Ty::Int)
                        }
                        Ty::Cell | Ty::Nested => (
                            format!(
                                "{}.{}.{}",
                                v.name,
                                self.pick(&["v", "n"]),
                                self.pick(&["x", "y"])
                            ),
                            Ty::Int,
                        ),
                        Ty::Vec => (format!("{}[0]", v.name), Ty::Int),
                        Ty::Rows => (format!("{}[0]", v.name), Ty::Value),
                        Ty::NestedRows => (format!("{}[0].v", v.name), Ty::Value),
                        Ty::Map => (format!("{}[\"a\"]", v.name), Ty::Value),
                        _ => (v.name.clone(), v.ty),
                    };
                    writeln!(out, "{place} = {}", self.expr(env, ty, 2)).unwrap();
                    self.trace_variable(out, &v);
                }
                2 if depth < 2 => {
                    writeln!(out, "if {} {{", self.expr(env, Ty::Bool, 2)).unwrap();
                    self.body(out, &mut env.clone(), budget, depth + 1, in_loop, coroutine);
                    out.push_str("} else {\n");
                    self.body(out, &mut env.clone(), budget, depth + 1, in_loop, coroutine);
                    out.push_str("}\n");
                }
                3 | 4 if depth < 2 => {
                    let index = self.fresh();
                    let limit = self.pick(&[1, 2, 3]);
                    let mut inner = env.clone();
                    if action == 3 {
                        writeln!(out, "for {index} in 0..{limit} {{").unwrap();
                        inner.push(Variable {
                            name: index.clone(),
                            ty: Ty::Int,
                            counter: true,
                        });
                    } else {
                        // The guard is private to the grammar: generated assignments cannot reset it.
                        writeln!(
                            out,
                            "let {index} = 0\nwhile {index} < {limit} {{\n{index} += 1"
                        )
                        .unwrap();
                    }
                    self.body(out, &mut inner, budget, depth + 1, true, coroutine);
                    out.push_str("}\n");
                }
                5 => {
                    let event = self.expr(env, Ty::Event, 1);
                    writeln!(out, "match {event} {{").unwrap();
                    for (variant, ty) in self.variants.clone() {
                        let name = self.fresh();
                        writeln!(out, "{variant}({name}) => {{").unwrap();
                        self.trace_typed(out, &name, ty);
                        let mut inner = env.clone();
                        inner.push(Variable {
                            name,
                            ty,
                            counter: false,
                        });
                        if depth < 2 {
                            self.body(out, &mut inner, budget, depth + 1, in_loop, coroutine);
                        }
                        out.push_str("},\n");
                    }
                    out.push_str("Empty => { print(\"empty\") },\n}\n");
                }
                7 if !coroutine => {
                    let candidates: Vec<_> = env
                        .iter()
                        .filter(|v| matches!(v.ty, Ty::Value | Ty::Flat | Ty::Cell | Ty::Nested))
                        .cloned()
                        .collect();
                    if !candidates.is_empty() {
                        let v = candidates[self.pick(&(0..candidates.len()).collect::<Vec<_>>())]
                            .clone();
                        let (root, ty) = match v.ty {
                            Ty::Cell | Ty::Nested => (format!("{}.v", v.name), Ty::RefValue),
                            Ty::Value => (v.name.clone(), Ty::RefValue),
                            _ => (v.name.clone(), Ty::RefFlat),
                        };
                        let alias = self.fresh();
                        writeln!(
                            out,
                            "let {alias}: {} = {root}\n{alias}.x = {}",
                            ty.rock(),
                            self.expr(env, Ty::Int, 2)
                        )
                        .unwrap();
                        self.trace(out, &root);
                        env.push(Variable {
                            name: alias,
                            ty,
                            counter: false,
                        });
                    }
                }
                8 => {
                    if let Some(v) = self.variable(env, Ty::Int) {
                        let f = self.fresh();
                        let rhs = if v.counter {
                            format!("{} + 1", v.name)
                        } else {
                            format!("({} % 10000) + {}", v.name, self.pick(&[-3, 1, 7]))
                        };
                        writeln!(
                            out,
                            "let {f}: fn() -> int = () => {{ {} = {rhs}; return {} }}",
                            v.name, v.name
                        )
                        .unwrap();
                        if let Some(functions) = self.variable(env, Ty::Functions) {
                            writeln!(out, "{}.push({f})", functions.name).unwrap();
                        }
                        self.trace(out, &format!("{f}()"));
                        self.trace_variable(out, &v);
                        env.push(Variable {
                            name: f,
                            ty: Ty::Closure,
                            counter: false,
                        });
                    }
                }
                9 => {
                    let ty =
                        self.pick(&[Ty::Vec, Ty::Rows, Ty::NestedRows, Ty::Map, Ty::Functions]);
                    if let Some(v) = self.variable(env, ty) {
                        let item = self.expr(
                            env,
                            if ty == Ty::Vec {
                                Ty::Int
                            } else if ty == Ty::NestedRows {
                                Ty::Nested
                            } else if ty == Ty::Functions {
                                Ty::Closure
                            } else {
                                Ty::Value
                            },
                            1,
                        );
                        if ty == Ty::Map {
                            let key = self.pick(&["a", "b", "__proto__"]);
                            writeln!(out, "{}[\"{key}\"] = {item}", v.name).unwrap();
                            if self.pick(&[true, false]) {
                                writeln!(out, "{}.remove(\"a\")", v.name).unwrap();
                            }
                        } else {
                            writeln!(out, "{}.push({item})", v.name).unwrap();
                        }
                        self.trace_variable(out, &v);
                    }
                }
                11 => {
                    let ty = self.pick(&[Ty::Value, Ty::Flat, Ty::Cell, Ty::Nested]);
                    if let Some(v) = self.variable(env, ty) {
                        let copy = self.fresh();
                        let field = if matches!(ty, Ty::Cell | Ty::Nested) {
                            "v.x"
                        } else {
                            "x"
                        };
                        let rhs = self.expr(env, Ty::Int, 1);
                        writeln!(
                            out,
                            "let {copy} = generic({}, false)\n{copy}.{field} = {rhs}",
                            v.name
                        )
                        .unwrap();
                        self.trace_variable(out, &v);
                        self.trace(out, &copy);
                    }
                }
                12 => {
                    let ty = self.pick(&[Ty::Value, Ty::Cell, Ty::MaybeValue, Ty::MaybeCell]);
                    let receiver = self.expr(env, ty, 0);
                    let arg = self.expr(env, Ty::Int, 1);
                    let receiver = if matches!(ty, Ty::MaybeValue | Ty::MaybeCell) {
                        format!("({receiver} : {})?", ty.rock())
                    } else {
                        format!("({receiver})")
                    };
                    let call = if self.pick(&[true, false]) {
                        format!("{receiver}.adjust({arg})")
                    } else {
                        format!("{receiver}.adjust({arg}, null)")
                    };
                    self.trace_typed(
                        out,
                        &call,
                        if matches!(ty, Ty::MaybeValue | Ty::MaybeCell) {
                            Ty::MaybeInt
                        } else {
                            Ty::Int
                        },
                    );
                }
                13 => {
                    let local = self.fresh();
                    let functions = self.fresh();
                    let index = self.fresh();
                    let start = self.expr(env, Ty::Int, 1);
                    let limit = self.pick(&[1, 2, 3]);
                    let step = self.pick(&[-3, 1, 7]);
                    writeln!(out, "let {local} = ({start} % 10000)\nlet {functions}: [fn() -> int] = []\nfor {index} in 0..{limit} {{\nlet captured = {index}\n{functions}.push(() => {{ {local} += {step}; return {local} + captured }})\n}}\n{local} = -{local}").unwrap();
                    self.trace(out, &functions);
                    self.trace(out, &local);
                }
                14 if coroutine => {
                    let local = self.fresh();
                    let closure = self.fresh();
                    let start = self.expr(env, Ty::Int, 1);
                    let event = self.expr(env, Ty::Event, 1);
                    let step = self.pick(&[-3, 1, 7]);
                    writeln!(out, "let {local} = ({start} % 10000)\nlet {closure} = () => {{ {local} += {step}; return {local} }}\nyield {event}").unwrap();
                    self.trace(out, &format!("{closure}()"));
                    writeln!(out, "yield {event}\n{local} = -{local}").unwrap();
                    self.trace(out, &format!("{closure}()"));
                    self.trace(out, &local);
                }
                10 if in_loop => {
                    writeln!(
                        out,
                        "if {} {{ {} }}",
                        self.expr(env, Ty::Bool, 1),
                        self.pick(&["break", "continue"])
                    )
                    .unwrap();
                }
                _ if coroutine => {
                    writeln!(out, "yield {}", self.expr(env, Ty::Event, 1)).unwrap();
                }
                _ => {
                    let ty = self.pick(&[Ty::Int, Ty::Value, Ty::Flat, Ty::String]);
                    let value = self.expr(env, ty, 2);
                    self.trace(out, &value);
                }
            }
        }
    }
}

pub(super) fn program(seed: u64) -> String {
    revised(seed, &[])
}

/// `program(seed)` with helper `i` drawn from revision `revisions[i]`.
pub(super) fn revised(seed: u64, revisions: &[u64]) -> String {
    let mut g = Generator {
        random: seed,
        serial: 0,
        helpers: Vec::new(),
        overloads: false,
        variants: Vec::new(),
    };
    let mut out = format!("// differential seed {seed}\n{RUNTIME}\n");
    out.push_str("enum Event {\n");
    for i in 0..g.pick(&[2, 3, 4]) {
        let ty = g.pick(&[
            Ty::Int,
            Ty::Uint,
            Ty::Bool,
            Ty::String,
            Ty::Value,
            Ty::Flat,
            Ty::Nested,
            Ty::MaybeInt,
            Ty::MaybeValue,
        ]);
        let name = format!("Case{i}");
        writeln!(out, "{name}({}),", ty.rock()).unwrap();
        g.variants.push((name, ty));
    }
    out.push_str("Empty\n}\nfn observe(tag: int, x: Event) {\nmatch x {\n");
    for (name, ty) in &g.variants {
        writeln!(out, "{name}(value) => {}(tag, value),", ty.observer()).unwrap();
    }
    out.push_str("Empty => print(\"{tag}=empty\")\n}\n}\n");
    // Generic bodies have only copy operations, which are valid for every T.
    let count = g.pick(&[1, 2, 3]);
    writeln!(out, "struct Slot<T> {{ item: T }}\nfn generic<T>(x: T, flag: bool) -> T {{\nlet saved: Slot<T> = Slot {{ item: x }}\nfor i in 0..{count} {{ if flag {{ return saved.item }} else {{ saved.item = x }} }}\nreturn saved.item\n}}").unwrap();
    for ty in [Ty::Int, Ty::Value, Ty::Flat] {
        let mut env = vec![Variable {
            name: "x".into(),
            ty,
            counter: false,
        }];
        let mut body = String::new();
        g.body(&mut body, &mut env, &mut 4, 0, false, false);
        let result = g.expr(&env, Ty::Int, 1);
        writeln!(
            out,
            "{}fn over(x: {}) -> int {{\n{body}\nreturn {result}\n}}",
            g.pick(&["", "@inline "]),
            ty.rock()
        )
        .unwrap();
        // Overloads cannot call themselves during generation.
    }
    g.overloads = true;
    for i in 0..g.pick(&[2, 3, 4]) {
        let ty = g.pick(&[
            Ty::Int,
            Ty::Uint,
            Ty::Float,
            Ty::Bool,
            Ty::String,
            Ty::Value,
            Ty::Flat,
        ]);
        let inline = g.pick(&["", "@inline "]);
        // A helper body draws from its own stream, so a revision rewrites that
        // body alone and leaves the rest of the program byte-identical.
        let saved = (g.random, g.serial);
        let revision = revisions.get(i).copied().unwrap_or(0);
        g.random = seed ^ (i as u64 + 1).wrapping_mul(0x517c_c1b7_2722_0a95) ^ (revision << 32);
        g.serial = 100_000 * (i + 1);
        let mut env = vec![Variable {
            name: "input".into(),
            ty: Ty::Int,
            counter: false,
        }];
        let mut body = String::new();
        g.body(&mut body, &mut env, &mut 6, 0, false, false);
        let result = g.expr(&env, ty, 2);
        (g.random, g.serial) = saved;
        writeln!(
            out,
            "{inline}fn helper{i}(input: int) -> {} {{\n{body}\nreturn {result}\n}}",
            ty.rock()
        )
        .unwrap();
        g.helpers.push((format!("helper{i}"), ty));
    }
    let mut env = vec![Variable {
        name: "input".into(),
        ty: Ty::Int,
        counter: false,
    }];
    let mut body = String::new();
    for ty in [Ty::Int, Ty::Value, Ty::Flat] {
        g.bind(&mut body, &mut env, ty);
    }
    g.body(&mut body, &mut env, &mut 16, 0, false, true);
    writeln!(
        out,
        "co fn stream(input: int) -> Coroutine<Event> {{\n{body}\nyield {}\n}}",
        g.expr(&env, Ty::Event, 1)
    )
    .unwrap();
    env = vec![Variable {
        name: "input".into(),
        ty: Ty::Int,
        counter: false,
    }];
    body.clear();
    // Randomize declaration order as well as the subsequent statement tree.
    let mut types = vec![
        Ty::Int,
        Ty::Uint,
        Ty::Float,
        Ty::Bool,
        Ty::String,
        Ty::Value,
        Ty::Flat,
        Ty::Cell,
        Ty::Nested,
        Ty::MaybeInt,
        Ty::MaybeValue,
        Ty::MaybeCell,
        Ty::Vec,
        Ty::Rows,
        Ty::NestedRows,
        Ty::Map,
        Ty::Functions,
    ];
    while !types.is_empty() {
        let index = g.pick(&(0..types.len()).collect::<Vec<_>>());
        g.bind(&mut body, &mut env, types.remove(index));
    }
    let initial_bias = seed % 29 + 1;
    let replacement_bias = seed % 37 + 200;
    let null_period = seed % 3 + 2;
    writeln!(
        body,
        "let snapshot_point: SnapshotPoint? = if input % {null_period} == 0 {{ null }} else {{ SnapshotPoint {{ x: (input % 10000) + {initial_bias}, y: 2 }} }}\nlet snapshot_owner = SnapshotOwner {{ record: SnapshotRecord {{ child: snapshot_point, refs: [input] }} }}\ncheck_nullable_child_snapshot(snapshot_owner, (input % 10000) + {replacement_bias})"
    )
    .unwrap();
    g.body(&mut body, &mut env, &mut 28, 0, false, false);
    for v in &env {
        g.trace_variable(&mut body, v);
    }
    writeln!(out, "fn probe(input: int) {{\n{body}\nlet c = stream(input)\nfor step in 0..128 {{\nif let e = c.resume() {{ observe(step, e) }} else {{ break }}\n}}\n}}\nfn main() {{\nassert(jit_enabled())\ncheck_generated_loops()\ncheck_generated_json()\nfor round in 0..64 {{ probe(round) }}\nfor round in 0..80 {{ check_areas() }}\n}}").unwrap();
    super::oracle::loops(&mut g, &mut out);
    super::oracle::json(&mut g, &mut out);
    super::areas::areas(&mut g, &mut out);
    out
}

const RUNTIME: &str = r#"
@nomangle inline(lua) fn jit_enabled() -> bool = """return jit.status() or os.getenv("ROCK_DIFF_JIT") == 'off'"""
@nomangle inline(js) fn jit_enabled() -> bool = """return true;"""
fn label(x: int) -> string { let n = if x == 0 { 0 } else { x }; return "{n}" }
fn label(x: uint) -> string { return "{x}" }
fn label(x: bool) -> string { return "{x}" }
value V { x: int, y: int }
nobox value N { x: int, y: int }
struct Cell { v: V, n: N }
value Nested { v: V, n: N }
value SnapshotPoint { x: int, y: int }
value SnapshotRecord { child: SnapshotPoint?, refs: [int] }
struct SnapshotOwner { record: SnapshotRecord }
fn check_nullable_child_snapshot(owner: SnapshotOwner, replacement: int) {
    let expected = owner.record.child?.x ?? -1
    let saved = owner.record
    if owner.record.child != null { owner.record.child.x = replacement }
    assert((saved.child?.x ?? -1) == expected, "nullable child snapshot changed")
}
@inline fn V.adjust(self, x: int, step: int? = 3) -> int {
    if x < 0 { return self.x }
    return (self.y % 10000) + (x % 10000) + (step ?? -1)
}
fn Cell.adjust(self, x: int, step: int? = 2) -> int {
    if x == 0 { return self.n.x }
    self.v.x = (self.v.x % 10000) + (step ?? -1)
    return self.v.adjust(x, step)
}
fn observe(tag: int, x: int) { print(tag); print(if x == 0 { 0 } else { x }) }
fn observe(tag: int, x: uint) { print(tag); print(x) }
fn observe(tag: int, x: float) { print(tag); print(if x == 0.0 { 0.0 } else { x }) }
fn observe(tag: int, x: bool) { print("{tag}={x}") }
fn observe(tag: int, x: string) { print("{tag}={x}") }
fn observe(tag: int, x: V) { observe(tag, x.x); observe(tag, x.y) }
fn observe(tag: int, x: N) { observe(tag, x.x); observe(tag, x.y) }
fn observe(tag: int, x: Cell) { observe(tag, x.v); observe(tag, x.n) }
fn observe(tag: int, x: Nested) { observe(tag, x.v); observe(tag, x.n) }
fn observe_optional(tag: int, x: int?) { observe(tag, x == null); observe(tag, x ?? 0) }
fn observe_optional(tag: int, x: V?) { observe(tag, x == null); observe(tag, x?.x ?? 0); observe(tag, x?.y ?? 0) }
fn observe_optional(tag: int, x: Cell?) { observe(tag, x == null); observe(tag, x?.v?.x ?? 0); observe(tag, x?.n?.y ?? 0) }
fn observe(tag: int, x: [int]) { observe(tag, x.len()); observe(tag, x[0]) }
fn observe(tag: int, x: [V]) { observe(tag, x.len()); observe(tag, x[0]) }
fn observe(tag: int, x: [Nested]) { observe(tag, x.len()); observe(tag, x[0]) }
fn observe(tag: int, x: Map<string, V>) {
    observe(tag, x.len())
    for key in ["a", "b", "__proto__"] { if let v = x.get(key) { observe(tag, v) } }
}
fn observe(tag: int, x: fn() -> int) { observe(tag, x()) }
fn observe(tag: int, x: [fn() -> int]) { for f in x { observe(tag, f()) } }
"#;
