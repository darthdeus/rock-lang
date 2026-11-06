Hover behavior on a live server after char-by-char typing. The one-shot CLI
(`rock lsp hover`) resolves both positions correctly; only the incremental
server path diverges, so these must stay edit_sequence tests.

TEST: hover_iteratee_after_typing_for_loop
TYPE: edit_sequence
---
STEP 1:
struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {

}
---
CHECK: hover(8, 5)
EXPECTED:
Mob
---
STEP 2 (type at 9:4):
for a in m.aggressors {
    print(a.dmg)
}
---
CHECK: hover(9, 18)
EXPECTED:
[AggressorEntry]
===

PINNED BUG (sentinel): after typing a for-loop into a live server, hover on
the NEW loop binding (`a`) returns an empty hover; the correct answer is
"Type: struct AggressorEntry" (the one-shot CLI and a non-typed live session
both return it -- see hover_for_binding_fresh below). The `EXPECTED: none`
below encodes today's wrong behavior. If this test FAILS, the incremental
hover bug was fixed: change `none` to `AggressorEntry` and delete this note.

TEST: hover_new_for_binding_after_typing_sentinel
TYPE: edit_sequence
---
STEP 1:
struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {

}
---
CHECK: hover(8, 5)
EXPECTED:
Mob
---
STEP 2 (type at 9:4):
for a in m.aggressors {
    print(a.dmg)
}
---
CHECK: hover(9, 8)
EXPECTED:
none
===

TEST: hover_for_binding_fresh
TYPE: edit_sequence
---
STEP 1:
struct AggressorEntry {
    dmg: int
}

struct Mob {
    aggressors: [AggressorEntry] = [],
}

fn f(m: Mob) {
    for a in m.aggressors {
        print(a.dmg)
    }
}
---
CHECK: hover(9, 8)
EXPECTED:
AggressorEntry
===
