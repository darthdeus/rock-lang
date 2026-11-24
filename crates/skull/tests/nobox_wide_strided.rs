use crate::common::{emit_module, run_luajit};

const GENERIC_WIDE: &str = r#"
nobox value Wide<T> { a: int, item: T, c: int, d: int, flag: bool }
value Pair { x: int, y: int }
global STORAGE: [int] = []
fn append<T>(xs: [Wide<T>], item: T) {
    xs.push(Wide { a: 1, item, c: 3, d: 4, flag: false })
}
inline(lua) fn row_width(xs: any) -> int = "return xs['@vec'].repr == 'strided' and xs['@vec'].stride or -1"
fn main() {
    let ints: [Wide<int>] = []
    let pairs: [Wide<Pair>] = []
    let arrays: [Wide<[int]>] = []
    let absent: [Wide<int?>] = []
    append(ints, 7)
    append(pairs, Pair { x: 11, y: 12 })
    append(arrays, [21])
    append(absent, null)
    print("{row_width(ints)} {row_width(pairs)} {row_width(arrays)} {row_width(absent)}")
    arrays[0].item.push(22)
    print("{ints[0].item} {pairs[0].item.x} {pairs[0].item.y} {arrays[0].item.len()} {absent[0].item ?? -1} {ints[0].flag}")
}
"#;

#[test]
fn wide_generic_rows_splice_value_arguments_like_a_concrete_row() {
    let lua = emit_module(GENERIC_WIDE);
    assert_eq!(
        run_luajit(&format!(
            "{lua}\nmain()\nlocal row = __ROCK_STRIDED_ROWS.Wide\nassert(row.a == 1 and row.item == 2 and row.flag == 5)\n"
        )),
        "5 6 5 5\n7 11 12 2 -1 false",
    );
}
