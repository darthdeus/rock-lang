References after a body edit that shifts lines: add/remove a line in one fn,
then query a symbol in ANOTHER fn -- the returned spans must use post-edit
offsets. Exercises the real JSON-RPC server across didChange, which the
in-process BoneWorkspace tests cannot (rock-lsp glue, versioning, threading).

TEST: references_shift_after_typing_line_in_other_fn
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
===

TEST: references_shift_after_removing_line_in_other_fn
TYPE: edit_sequence
---
STEP 1:
fn alpha() -> int {
    let pad = 0
    return 1
}

fn beta() -> int {
    let count = 2
    return count + count
}
---
CHECK: references(6, 8)
EXPECTED:
6:8
7:11
7:19
---
STEP 2:
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
fn alpha() -> int {
    let pad = 0
    return 1 + pad
}

fn beta() -> int {
    return alpha() + alpha()
}
---
CHECK: references(6, 12)
EXPECTED:
0:3
6:11
6:21
===
