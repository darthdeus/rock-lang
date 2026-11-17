use crate::common::{function_body, run_main, try_emit_module};

const WIDE_NOBOX_OVERWRITE: &str = r#"
nobox value Wide { a: int, b: int, c: int, d: int, e: int }

fn main() {
    let values: [Wide] = []
    values.push(Wide { a: 1, b: 2, c: 3, d: 4, e: 5 })
    values[0] = Wide { a: 6, b: 7, c: 8, d: 9, e: 10 }
    print("{values[0].a} {values[0].e}")
}
"#;

#[test]
fn wide_nobox_vec_stays_flat_past_the_implicit_threshold() {
    let lua = try_emit_module(WIDE_NOBOX_OVERWRITE)
        .expect("an explicit nobox guarantee must not fall back to boxed Vec elements");
    let body = function_body(&lua, "main");
    assert!(!body.contains("Wide_lct("));
    assert!(crate::common::has_vec_header(body, "strided", None));
    assert_eq!(run_main(&lua), "6 10");
}

fn source(extra: usize) -> String {
    let names = ["a", "b", "c", "d"];
    let fields: Vec<String> = (0..extra)
        .map(|i| format!(", {}: float", names[i]))
        .collect();
    let inits: Vec<String> = (0..extra)
        .map(|i| format!(", {}: {}.0", names[i], i + 3))
        .collect();
    let reads: Vec<String> = (0..extra)
        .map(|i| format!(" {{V[0].{}}}", names[i]))
        .collect();
    format!(
        r#"
nobox value NV2 {{ x: float, y: float }}
value Opt {{ pos: NV2?{f} }}
global V: [Opt] = []
fn main() {{
    V.push(Opt {{ pos: NV2 {{ x: 1.0, y: 2.0 }}{i} }})
    V.push(Opt {{ pos: null{i} }})
    print("{{V[0].pos?.x ?? -1.0}} {{V[0].pos?.y ?? -1.0}}{r}")
    print("{{V[1].pos?.x ?? -1.0}} {{V[1] == V[0]}}")
}}
"#,
        f = fields.concat(),
        i = inits.concat(),
        r = reads.concat(),
    )
}

fn is_column_stored(lua: &str) -> bool {
    !lua.lines().any(|l| l.matches("Opt_lct(").count() == 1)
}

#[test]
fn every_leaf_count_round_trips_and_the_storage_tier_flips_at_the_threshold() {
    let mut rows = Vec::new();
    for extra in 0..5 {
        let leaves = extra + 3;
        let src = source(extra);
        let lua = match try_emit_module(&src) {
            Ok(lua) => lua,
            Err(e) => panic!("{} fields / {leaves} leaves must emit: {e:?}", extra + 1),
        };
        let want_present: String = std::iter::once("1 2".to_string())
            .chain((0..extra).map(|i| format!(" {}", i + 3)))
            .collect();
        let got = run_main(&lua);
        let got: Vec<&str> = got.trim().lines().collect();
        assert_eq!(got, vec![want_present.as_str(), "-1 false"]);
        rows.push((leaves, is_column_stored(&lua)));
    }
    // The flip itself: at or below 4 leaves the element is columns, above it a
    // box. Asserted as the whole sweep so a moved constant reads as one diff.
    assert_eq!(
        rows,
        vec![(3, true), (4, true), (5, false), (6, false), (7, false)]
    );
}
