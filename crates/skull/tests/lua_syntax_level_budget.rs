//! LuaJIT's parser caps expression and block nesting at 200 syntax levels
//! (`LJ_MAX_XLEVEL`, `synlevel_begin` in `lj_parse.c`). Every `..` of a concat
//! run is one level (concat is right-associative, so the parser recurses per
//! operand), and so is every parenthesized operand. Past the cap the whole
//! chunk fails to load with "chunk has too many syntax levels" and no Rock
//! location, so no ordinary program may emit an expression whose depth grows
//! with a field count or a statement count.
//!
//! Two emitters grew that way: the generated display helpers (`__tostring` and
//! the C-struct `inspect`) concatenated three operands per field into one run,
//! and the Lua emitter folded single-use values into their user without bound,
//! so a wide struct interpolation became one long `..` run and a sequence of
//! `total += x` became one expression with a parenthesis per statement.

use crate::common::{function_body, luajit_loads, require_luajit, run_luajit, try_emit_module};

/// A program with one value of each display tier, `n` fields wide: a cdata
/// value struct, a GC-table value struct, a leaf-table value struct, GC values
/// holding a `nobox` child inline (plain, and nullable absent and present), and
/// a cdata enum with an `n`-wide tuple variant. Each is shown from its own function so no frame holds
/// more than one wide value.
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

/// What [`display_tiers`] prints, built field by field.
fn display_tiers_expected(n: usize) -> String {
    let join = |f: &dyn Fn(usize) -> String| (0..n).map(f).collect::<Vec<_>>().join(", ");
    let floats = join(&|i| format!("f{i}: {i}.5"));
    let cdata = format!("CdataWide {{ {floats} }}");
    let inline = format!("InlineWide {{ {floats} }}");
    [
        cdata.clone(),
        cdata,
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

/// The rendering of narrow values is a contract: pinned literally, not derived.
#[test]
fn narrow_display_helpers_render_unchanged() {
    require_luajit();
    let lua = try_emit_module(&display_tiers(2)).expect("emits");
    let expected = "CdataWide { f0: 0.5, f1: 1.5 }\n\
                    CdataWide { f0: 0.5, f1: 1.5 }\n\
                    GcWide { g0: s0, g1: s1 }\n\
                    LeafWide { pos: true, pos.x: 1, pos.y: 2, l0: 0.25, l1: 1.25 }\n\
                    Holder { tag: t, inner: InlineWide { f0: 0.5, f1: 1.5 } }\n\
                    MaybeHolder { tag: t, maybe: nil }\n\
                    MaybeHolder { tag: t, maybe: InlineWide { f0: 0.5, f1: 1.5 } }\n\
                    Wide::Tuple { [1] = 0, [2] = 1 }";
    assert_eq!(expected, display_tiers_expected(2));
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "narrow_display_helpers"),
        expected
    );
}

#[test]
fn wide_display_helpers_load_and_render() {
    require_luajit();
    let n = 100;
    let lua = try_emit_module(&display_tiers(n)).expect("emits");
    luajit_loads(&lua, "wide_display_helpers").expect("the emitted chunk must load");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "wide_display_helpers"),
        display_tiers_expected(n)
    );
}

/// Interpolating a value renders it field by field inside the calling
/// function, so the concat run is the caller's own expression.
#[test]
fn wide_value_interpolation_loads_and_renders() {
    require_luajit();
    let n = 70;
    let fields = (0..n)
        .map(|i| format!("f{i}: int = {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let src =
        format!("value Row {{ {fields} }}\nfn main() {{ let r = Row {{}}; print(\"<{{r}}>\") }}\n");
    let lua = try_emit_module(&src).expect("emits");
    luajit_loads(&lua, "wide_value_interpolation").expect("the emitted chunk must load");
    let body = (0..n)
        .map(|i| format!("f{i}: {i}"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "wide_value_interpolation"),
        format!("<Row {{ {body} }}>")
    );
}

/// `sum(p0..p9)` adding the parameters cyclically in `statements` statements.
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

/// Each `total += p` folds into the next, so without a bound the function
/// returned one expression nested one parenthesis per statement.
#[test]
fn long_accumulation_loads_and_runs() {
    require_luajit();
    let lua = try_emit_module(&accumulation(300)).expect("emits");
    luajit_loads(&lua, "long_accumulation").expect("the emitted chunk must load");
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "long_accumulation"),
        "1650"
    );
}

/// The bound only splits what LuaJIT could not load: an accumulation well
/// within the cap still folds into one expression.
#[test]
fn accumulation_within_the_budget_stays_one_expression() {
    let lua = try_emit_module(&accumulation(120)).expect("emits");
    let body = function_body(&lua, "sum");
    let sums = body.lines().filter(|line| line.contains(" + ")).count();
    assert_eq!(sums, 1, "a 120-term sum was split:\n{body}");
}
