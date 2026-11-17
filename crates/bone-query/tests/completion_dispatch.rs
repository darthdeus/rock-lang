//! Pins the exact ordered output of every completion context the dispatcher
//! routes to: each case renders the head of the response (label, sort text,
//! insert text) so a restructuring of the dispatch cannot silently reorder,
//! re-route or re-rank a context.

use crate::completion::complete_in;
use lsp_types::CompletionItem;

const PATH: &str = "/tmp/rock_lsp_bone_completion_dispatch.rock";
const HEAD: usize = 8;

/// One line per item for the first `HEAD` items, then the total count (or
/// `more` when the head is truncated).
fn render(items: &[CompletionItem]) -> String {
    let mut out = String::new();
    for it in items.iter().take(HEAD) {
        let edit = if it.additional_text_edits.is_some() {
            " | +edit"
        } else {
            ""
        };
        out.push_str(&format!(
            "{} | {} | {}{edit}\n",
            it.label,
            it.sort_text.as_deref().unwrap_or("-"),
            it.insert_text.as_deref().unwrap_or("-"),
        ));
    }
    // Past the head the tail is mostly stdlib; pin only that it exists.
    if items.len() <= HEAD {
        out.push_str(&format!("total {}\n", items.len()));
    } else {
        out.push_str("more\n");
    }
    out
}

#[test]
fn every_dispatch_context_renders_its_pinned_head() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "member access resolved by inference",
            "struct V { x: float, y: float }\nfn dist(a: V) -> float { a.x }\n\
             fn r() {\n  let v = V { x: 1.0, y: 2.0 }\n  v.<|>\n}\n",
            "\
             dist | 100001009999000000dist | dist()\n\
             x: float | 100001009999000000x | x\n\
             y: float | 100001009999000000y | y\n\
             total 3\n",
        ),
        // `r(thing: Thing)` takes a `Thing` first, so `thing.r()` is a valid
        // (self-recursive) UFCS call; both member paths offer it.
        (
            "member access name walk fallback",
            "struct Thing { hp: int, mp: int }\n\
             fn r(thing: Thing) {\n  newHelper(thing.<|>)\n}\n",
            "\
             hp: int | 100001009999000000hp | hp\n\
             mp: int | 100001009999000000mp | mp\n\
             r | 100001009999000000r | r()\n\
             total 3\n",
        ),
        (
            "member access offers the enclosing fn, inferred",
            "struct V { x: float }\nfn r(v: V) {\n  v.<|>\n}\n",
            "\
             r | 100001009999000000r | r()\n\
             x: float | 100001009999000000x | x\n\
             total 2\n",
        ),
        (
            "member access offers the enclosing fn, name walk",
            "struct V { x: float }\nfn r(v: V) {\n  newHelper(v.<|>)\n}\n",
            "\
             r | 100001009999000000r | r()\n\
             x: float | 100001009999000000x | x\n\
             total 2\n",
        ),
        (
            "member access on a non member receiver falls through to general",
            "fn scale(x: int) -> int { return x }\nfn sc_two() {}\n\
             fn r(x: int) {\n  let n = scale(x).sc<|>\n}\n",
            "\
             scale(x) | 100000103967000008scale(x) | scale(x)\n\
             scale | 100200108767020005scale | scale($0)\n\
             sc_two | 100200108767020006sc_two | sc_two()\n\
             scene_close | 100200108767320011scene_close | scene_close($0)\n\
             scene_create | 100200108767320012scene_create | scene_create($0)\n\
             scene_discard | 100200108767320013scene_discard | scene_discard($0)\n\
             scene_on_close | 100200108767320014scene_on_close | scene_on_close($0)\n\
             scene_live_count | 100200108767320016scene_live_count | scene_live_count()\n\
             more\n",
        ),
        (
            "field ref arg",
            "struct P { hp: int, name: string, mp: int }\n\
             fn watch(f: FieldRef<P, int>) {}\n\
             fn r() {\n  watch(.<|>)\n}\n",
            "\
             .hp: int | 100001009999000000hp | hp\n\
             .mp: int | 100001009999000000mp | mp\n\
             total 2\n",
        ),
        (
            "enum record literal fields",
            "enum Shape { Rect { width: int, height: int }, Dot }\n\
             fn r() {\n  let s = Shape::Rect { <|> }\n}\n",
            "\
             width: int | 100000009999000000width | width\n\
             height: int | 100001009999000000height | height\n\
             total 2\n",
        ),
        (
            "enum qualified path",
            "enum Mode { Idle, Run(int), Jump { h: int } }\n\
             fn r() {\n  let m = Mode::<|>\n}\n",
            "\
             Idle | 100000009999000000idle | -\n\
             Run | 100001009999000000run | Run($0)\n\
             Jump | 100002009999000000jump | Jump { $0 }\n\
             total 3\n",
        ),
        (
            "struct literal fields",
            "struct Cfg { width: int, height: int = 2, title: string }\n\
             fn r() {\n  let c = Cfg { width: 1, <|> }\n}\n",
            "\
             title: string | 100002009999000000title | title\n\
             height: int | 110001009999000000height | height\n\
             total 2\n",
        ),
        (
            "decorator namespace",
            "deco init late_draw {}\ndeco init late_update {}\n@late<|>\nfn ui() {}\n",
            "\
             late_draw | 100000000000000009late_draw | late_draw\n\
             late_update | 100000000000000011late_update | late_update\n\
             total 2\n",
        ),
        (
            "general typed arg with reach and call through",
            "enum Err { NotGround, Blocked }\nstruct Mov { grid: int }\n\
             struct Pl { mov: Mov }\nfn reject(e: Err) -> int { return 0 }\n\
             fn take(g: int) {}\nfn r(pl: Pl, gx: int) {\n  take(g<|>)\n}\n",
            "\
             g: | 000000008978000001g | g: \n\
             gx | 100000008878000002gx | -\n\
             pl.mov.grid: int | 102002009988000011pl.mov.grid | pl.mov.grid\n\
             get | 110200108878300003get | get($0)\n\
             gsub | 110200108878320004gsub | gsub($0)\n\
             get_at | 110200108878320006get_at | get_at($0)\n\
             get_os | 110200108878320006get_os | get_os()\n\
             gmatch | 110200108878320006gmatch | gmatch($0)\n\
             more\n",
        ),
        (
            "general call through in expression position",
            "enum Err { NotGround, Blocked }\nfn reject(e: Err) -> int { return 0 }\n\
             fn r() {\n  rejnog<|>\n}\n",
            "\
             reject(Err::NotGround) | 142300109933000022reject(err::notground) | reject(Err::NotGround)\n\
             total 1\n",
        ),
        (
            "general untyped expression with macro",
            "macro twice(x: expr) {\n    x + x\n}\nfn tw_fn() {}\n\
             fn r(two: int) {\n  tw<|>\n}\n",
            "\
             two | 100000008767000003two | -\n\
             twice | 100200008767020005twice | twice($0)\n\
             tw_fn | 100200108767020005tw_fn | tw_fn()\n\
             TWO_PI | 101100028767320006two_pi | -\n\
             table_new_with_size(two) | 102000104967000024table_new_with_size(two) | table_new_with_size(two)\n\
             to_float(two) | 102100109977000013to_float(two) | to_float(two)\n\
             to_string(two) | 102100109977000014to_string(two) | to_string(two)\n\
             log_throttled_with_now | 102200009977320022log_throttled_with_now | log_throttled_with_now($0)\n\
             more\n",
        ),
        (
            "general match arm pattern head",
            "enum Mode { Idle, Moving }\nfn moo() {}\n\
             fn r(m: Mode) {\n  match m {\n    Mo<|>\n  }\n}\n",
            "\
             Mode::Idle | 100300008767020010mode::idle | -\n\
             Mode::Moving | 100300008767020012mode::moving | -\n\
             moo | 121200108767020003moo | moo()\n\
             mat4_ortho | 122200109867320010mat4_ortho | mat4_ortho($0)\n\
             total 4\n",
        ),
        (
            "general for in slot",
            "fn items() -> [int] { return [] }\nfn r(it: int, its: [int]) {\n  for x in it<|>\n}\n",
            "\
             it | 020000008767001002it | -\n\
             its | 100000008767000003its | -\n\
             int_to_float(it) | 102000104967000016int_to_float(it) | int_to_float(it)\n\
             is_ascii_ws(it) | 102100109977000015is_ascii_ws(it) | is_ascii_ws(it)\n\
             items | 110200108767020005items | items()\n\
             iter | 120200108767320004iter | iter($0)\n\
             MapIter | 122200009987310007mapiter | -\n\
             VecIter | 122200009987310007veciter | -\n\
             more\n",
        ),
        (
            "general param type entity hint",
            "struct Mob { health: int }\nco fn mob_attack_co(e: <|>) -> Coroutine<int> {\n  yield 1\n}\n",
            "\
             Entity<Mob> | 000000000000000011entity<mob> | Entity<Mob>\n\
             Mob | 100200009999020000mob | -\n\
             Arena | 100200009999300000arena | -\n\
             ArenaStore | 100200009999300000arenastore | -\n\
             Entity | 100200009999300000entity | -\n\
             JsonReader | 100200009999300000jsonreader | -\n\
             JsonValue | 100200009999300000jsonvalue | -\n\
             Mat4 | 100200009999300000mat4 | -\n\
             more\n",
        ),
        (
            "general named arg slot",
            "fn configure(width: int, height: int, title: string) {}\n\
             fn r(wide: int) {\n  configure(10, <|>)\n}\n",
            "\
             height: | 100000009999000000height | height: \n\
             title: | 100000009999000000title | title: \n\
             wide | 100000009999000000wide | -\n\
             get | 110200109999300000get | get($0)\n\
             len | 110200109999300000len | len($0)\n\
             parse_skip_ws_inline_at | 110200109999300000parse_skip_ws_inline_at | parse_skip_ws_inline_at($0)\n\
             phantom | 110200109999300000phantom | phantom()\n\
             unit_at | 110200109999300000unit_at | unit_at($0)\n\
             more\n",
        ),
        (
            "general coroutine type name snippets",
            "enum Wait { Frames(int) }\nco fn f() -> Cor<|> {\n  yield Wait::Frames(1)\n}\n",
            "\
             Coroutine<Wait> | 000000000000000015coroutine<wait> | Coroutine<Wait>\n\
             Coroutine<> | 000000000001000011coroutine<> | Coroutine<$0>\n\
             CanonicalUniformLeaf | 103200009866310020canonicaluniformleaf | -\n\
             CacheOwner | 103200019876310010cacheowner | -\n\
             JsonRecordCodec | 103200019976310015jsonrecordcodec | -\n\
             JsonRecordAdapter | 103200019976310017jsonrecordadapter | -\n\
             JsonRecordError | 103200019976320015jsonrecorderror | -\n\
             SemanticUniformBlock | 103200019986320020semanticuniformblock | -\n\
             more\n",
        ),
        (
            "general co fn return slot",
            "enum Wait { Frames(int) }\nco fn f()<|> {\n  yield Wait::Frames(1)\n}\n",
            "\
             -> Coroutine<Wait> | 000000000000000018-> coroutine<wait> | -> Coroutine<Wait>\n\
             PI | 100100009999310000pi | -\n\
             EPSILON | 100100009999320000epsilon | -\n\
             HALF_PI | 100100009999320000half_pi | -\n\
             PHI | 100100009999320000phi | -\n\
             QTR_PI | 100100009999320000qtr_pi | -\n\
             TAU | 100100009999320000tau | -\n\
             TWO_PI | 100100009999320000two_pi | -\n\
             more\n",
        ),
        (
            "general callee before existing parens",
            "fn world_to_screen(x: int) -> int { return x }\n\
             fn r() {\n  let a = world_to<|>(1)\n}\n",
            "\
             world_to_screen | 100200108091020015world_to_screen | world_to_screen\n\
             total 1\n",
        ),
    ];
    let mut failures = String::new();
    for (case, src, want) in cases {
        let got = render(&complete_in(PATH, src));
        if got != *want {
            failures.push_str(&format!("--- {case} ---\n{got}"));
        }
    }
    assert!(failures.is_empty(), "\n{failures}");
}
