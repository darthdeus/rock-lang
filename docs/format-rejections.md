# Formatting rejection evidence

Nine existing cases now assert their diagnostic: four composite numeric-format
flags and five unsupported structural payloads. Every original case already
rejected for its stated reason, with no competing error requiring isolation.
No production compiler code changed.

## Composite numeric flags

The [four cases](../tests/errors/composite_format_numeric_flags.rock.err) reject
explicit sign, zero-padding and grouping on an identity struct, plus zero-padding
on an array. These values use structural text rather than numeric presentation.
Width, alignment and fill remain valid for the rendered text.

[`DispatchCtx::format_plan`](../crates/skull-mir/src/lower/dispatch.rs) excludes
numeric flags from structural rendering. These cases have no inferred or explicit
printf conversion, so [`fmt_pieces`](../crates/skull-mir/src/lower/mod.rs) returns
`None`. [`lower_format_non_null`](../crates/skull-mir/src/lower/interp.rs) reports
`skull-mir: lowering gap: interpolation format specifier (grouping / fill / center)`.
The fixtures pin that existing text; it names a formatting boundary, not the
individual offending flag. A different failure at that same boundary could still
satisfy the assertion.

To isolate the numeric flag despite that diagnostic limit, all four original
programs were run after removing only their offending flag: `+12` and `012`
became `12`, `_` became an empty spec, and `08` became `8`. Each admitted and
matched its exact output on LuaJIT and JS (4/4 controls per backend). The two
struct-width controls print ` P { x = 1 }`, the empty-spec control prints
`P { x = 1 }`, and the array prints `Vec { 1, 2 }`; each ends with a newline.

The new [runtime controls](../tests/strings/format_rejection_controls.rock) retain
these flag-free boundaries with exact text assertions. The existing
[adversarial format corpus](../tests/strings/format_specifier_adversarial.rock)
checks accepted numeric sign/zero-padding and composite width/alignment/fill;
[grouping tests](../tests/strings/format_grouping.rock) check accepted scalar
numeric grouping. A generic valid-program replacement alone would not establish
which part of a rejected composite spec caused the failure.

## Structural payload limits

The [five cases](../tests/errors/enum_format_unsupported_payload.rock.err) assert
kind-specific reasons: an identity-struct enum payload, an array enum payload,
a callback enum payload, an array field in a value-record enum payload, and an
array field in a top-level value record.

[`formats_structurally` and `format_tree`](../crates/skull-mir/src/lower/format.rs)
select enums, value records and handles, then recursively check the logical
payload type. `nominal_tree` rejects identity structs when nested in this tree;
`format_tree_in` rejects containers and callbacks. This is narrower than a ban
on formatting these values anywhere: top-level identity structs and arrays use
`inspect`, as the accepted controls demonstrate.

[`structural_route` and `tostring_route`](../crates/skull-mir/src/lower/dispatch.rs)
carry the tree or rejection reason to interpolation and the stdlib string
conversion paths. [`unprintable_error`](../crates/skull-mir/src/lower/interp.rs)
adds the shared structural-formatter explanation. The
[enum runtime corpus](../tests/strings/enum_format.rock) covers scalar, string,
nullable, nested-enum, value-record and handle payloads through interpolation
and string conversion. The new runtime file adds a top-level scalar-only value
record beside the rejected container-bearing record.

## Validation and limits

```sh
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/composite_format_numeric_flags.rock.err --cases 4
python3 perf/evidence/check_operator_rejections.py --fixture tests/errors/enum_format_unsupported_payload.rock.err --cases 5
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/strings/format_specifier_adversarial.rock tests/strings/enum_format.rock tests/strings/format_rejection_controls.rock
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/strings/format_specifier_adversarial.rock tests/strings/enum_format.rock tests/strings/format_rejection_controls.rock --backend js
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/strings/format_grouping.rock
cargo run --quiet --locked -p rock-cli --bin rock -- test tests/strings/format_grouping.rock --backend js
```

The rejection checks pass all nine originals. Wrong-diagnostic and generic-valid
mutations each fail all nine assertions. The runtime checks pass 84/84 tests on
LuaJIT and 84/84 on JS: 56 adversarial, 17 enum, two new controls and nine grouping
tests. JS reports zero skipped and zero not-for-JS cases. Nine formerly unpinned
rejection cases are strengthened; the rejection case count is unchanged.

These are bounded examples, not exhaustive validation of format-spec syntax,
all structural payload kinds, every representation or nullable formatting. No
performance claim is made.
