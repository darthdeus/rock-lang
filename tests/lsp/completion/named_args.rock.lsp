TEST: named_arg_completion_at_call_site
TYPE: completion
---
fn greet(greeting: string, name: string) -> string {
    return greeting
}

fn main() {
    let x = greet(gr«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- greeting:
===

TEST: named_arg_completion_empty_prefix
TYPE: completion
---
fn configure(width: int, height: int, title: string) {
}

fn main() {
    configure(«cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- width:
- height:
- title:
===

TEST: named_arg_completion_skips_earlier_named
TYPE: completion
---
fn configure(width: int, height: int, title: string) {
}

fn main() {
    configure(width: 10, «cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- height:
- title:
===

TEST: named_arg_completion_skips_positional
TYPE: completion
---
fn configure(width: int, height: int, title: string) {
}

fn main() {
    configure(10, «cursor»)
}
---
OPERATION: completion
---
EXPECTED:
- height:
- title:
===
