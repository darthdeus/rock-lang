use crate::common::{emit_module, function_body, run_main};

fn display_tiers(n: usize) -> String {
    let list = |f: &dyn Fn(usize) -> String| (0..n).map(f).collect::<Vec<_>>().join(", ");
    let floats = list(&|i| format!("f{i}: float = {i}.5"));
    let strings = list(&|i| format!("g{i}: string = \"s{i}\""));
    let leaves = list(&|i| format!("l{i}: float = {i}.25"));
    let slots = list(&|_| "int".to_string());
    let args = list(&|i| i.to_string());
    format!(
        r#"
nobox value Pair {{ x: float = 1.0, y: float = 2.0 }}
value CdataWide {{ {floats} }}
value GcWide {{ {strings} }}
value LeafWide {{ pos: Pair? = Pair {{}}, {leaves} }}
nobox value InlineWide {{ {floats} }}
value Holder {{ tag: string = "t", inner: InlineWide = InlineWide {{}} }}
value MaybeHolder {{ tag: string = "t", maybe: InlineWide? = null }}
enum Wide {{ Tuple({slots}), Unit }}
fn show(a: any) {{ print(tostring(a)) }}
fn show_inspect(a: any) {{ print(inspect(a)) }}
fn show_cdata() {{ show(CdataWide {{}}) }}
fn show_cdata_inspect() {{ show_inspect(CdataWide {{}}) }}
fn show_gc() {{ show(GcWide {{}}) }}
fn show_leaf() {{ show(LeafWide {{}}) }}
fn show_holder() {{ show(Holder {{}}) }}
fn show_maybe_absent() {{ show(MaybeHolder {{}}) }}
fn show_maybe_present() {{ show(MaybeHolder {{ maybe: InlineWide {{}} }}) }}
fn show_enum() {{ show(Wide::Tuple({args})) }}
fn main() {{
    show_cdata()
    show_cdata_inspect()
    show_gc()
    show_leaf()
    show_holder()
    show_maybe_absent()
    show_maybe_present()
    show_enum()
}}
"#
    )
}

fn display_tiers_expected(n: usize) -> String {
    let join = |f: &dyn Fn(usize) -> String| (0..n).map(f).collect::<Vec<_>>().join(", ");
    let floats = join(&|i| format!("f{i}: {i}.5"));
    let cdata = format!("CdataWide {{ {floats} }}");
    let mut inspect_fields: Vec<String> = (0..n).map(|i| format!("f{i} = {i}.5")).collect();
    inspect_fields.sort_unstable();
    let cdata_inspect = if n <= 2 {
        format!("CdataWide {{ {} }}", inspect_fields.join(", "))
    } else {
        format!("CdataWide {{\n  {}\n}}", inspect_fields.join(",\n  "))
    };
    let inline = format!("InlineWide {{ {floats} }}");
    [
        cdata.clone(),
        cdata_inspect,
        format!("GcWide {{ {} }}", join(&|i| format!("g{i}: s{i}"))),
        format!(
            "LeafWide {{ pos: true, pos.x: 1, pos.y: 2, {} }}",
            join(&|i| format!("l{i}: {i}.25"))
        ),
        format!("Holder {{ tag: t, inner: {inline} }}"),
        "MaybeHolder { tag: t, maybe: nil }".to_string(),
        format!("MaybeHolder {{ tag: t, maybe: {inline} }}"),
        format!(
            "Wide::Tuple {{ {} }}",
            join(&|i| format!("[{}] = {i}", i + 1))
        ),
    ]
    .join("\n")
}

#[test]
fn narrow_display_helpers_render_expected_formats() {
    let lua = emit_module(&display_tiers(2));
    let expected = "CdataWide { f0: 0.5, f1: 1.5 }\n\
                    CdataWide { f0 = 0.5, f1 = 1.5 }\n\
                    GcWide { g0: s0, g1: s1 }\n\
                    LeafWide { pos: true, pos.x: 1, pos.y: 2, l0: 0.25, l1: 1.25 }\n\
                    Holder { tag: t, inner: InlineWide { f0: 0.5, f1: 1.5 } }\n\
                    MaybeHolder { tag: t, maybe: nil }\n\
                    MaybeHolder { tag: t, maybe: InlineWide { f0: 0.5, f1: 1.5 } }\n\
                    Wide::Tuple { [1] = 0, [2] = 1 }";
    assert_eq!(expected, display_tiers_expected(2));
    assert_eq!(run_main(&lua), expected);
}

#[test]
fn wide_display_helpers_load_and_render() {
    let n = 100;
    let lua = emit_module(&display_tiers(n));
    assert_eq!(run_main(&lua), display_tiers_expected(n));
}

#[test]
fn wide_value_interpolation_loads_and_renders() {
    let n = 70;
    let fields = (0..n)
        .map(|i| format!("f{i}: int = {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let src =
        format!("value Row {{ {fields} }}\nfn main() {{ let r = Row {{}}; print(\"<{{r}}>\") }}\n");
    let lua = emit_module(&src);
    let body = (0..n)
        .map(|i| format!("f{i}: {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(run_main(&lua), format!("<Row {{ {body} }}>"));
}

fn accumulation(statements: usize) -> String {
    let params = (0..10)
        .map(|i| format!("p{i}: int"))
        .collect::<Vec<_>>()
        .join(", ");
    let adds = (0..statements)
        .map(|i| format!("    total += p{}\n", i % 10))
        .collect::<String>();
    let args = (1..=10)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "fn sum({params}) -> int {{\n    let total = 0\n{adds}    return total\n}}\n\
         fn main() {{ print(\"{{sum({args})}}\") }}\n"
    )
}

#[test]
fn long_accumulation_loads_and_runs() {
    let lua = emit_module(&accumulation(300));
    assert_eq!(run_main(&lua), "1650");
}

#[test]
fn accumulation_within_the_budget_stays_one_expression() {
    let lua = emit_module(&accumulation(120));
    let body = function_body(&lua, "sum");
    let sums = body.lines().filter(|line| line.contains(" + ")).count();
    assert_eq!(sums, 1);
}
