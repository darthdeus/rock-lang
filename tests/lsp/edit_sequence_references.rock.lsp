References after a body edit that shifts lines: add/remove a line in one fn,
then query a symbol in ANOTHER fn -- the returned spans must use post-edit
offsets. Exercises the real JSON-RPC server across didChange, which the
in-process BoneWorkspace tests cannot (rock-lsp glue, versioning, threading).

TEST: references_shift_after_typing_and_removing_line_in_other_fn
TYPE: edit_sequence
---
STEP 1:
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    let count = 2
    return count + count
}
---
CHECK: references(5, 8)
EXPECTED:
5:8
6:11
6:19
---
STEP 2 (type at 1:12):

    let pad = 0
---
CHECK: references(6, 8)
EXPECTED:
6:8
7:11
7:19
---
STEP 3 (delete at 2:15, 16):
---
CHECK: references(5, 8)
EXPECTED:
5:8
6:11
6:19
===

TEST: enum_variant_references_shift_after_body_edit
TYPE: edit_sequence
---
STEP 1:
enum Color {
    Red,
    Green,
}

fn alpha() -> int {
    return 1
}

fn beta(c: Color) -> int {
    match c {
        Color::Red => 1,
        Color::Green => 2,
    }
}
---
CHECK: references(11, 15)
EXPECTED:
1:4
11:15
---
STEP 2:
SOURCE: 1
REPLACE: ["    return 1\n", "    let pad = 0\n    return 1 + pad\n"]
---
CHECK: references(12, 15)
EXPECTED:
1:4
12:15
===

TEST: call_site_references_shift_after_edit_in_other_fn
TYPE: edit_sequence
---
STEP 1:
fn alpha() -> int {
    return 1
}

fn beta() -> int {
    return alpha() + alpha()
}
---
CHECK: references(5, 12)
EXPECTED:
0:3
5:11
5:21
---
STEP 2:
SOURCE: 1
REPLACE: ["    return 1\n", "    let pad = 0\n    return 1 + pad\n"]
---
CHECK: references(6, 12)
EXPECTED:
0:3
6:11
6:21
===
