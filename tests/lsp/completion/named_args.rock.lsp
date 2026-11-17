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
EXPECTED:
- width:
- height:
- title:
===

TEST: named_arg_completion_skips_earlier_named
TYPE: completion
SOURCE: named_arg_completion_empty_prefix
REPLACE: ["    configure()", "    configure(width: 10, )"]
POSITION: line=4, character=25
EXPECTED:
- height:
- title:
===

TEST: named_arg_completion_skips_positional
TYPE: completion
SOURCE: named_arg_completion_empty_prefix
REPLACE: ["    configure()", "    configure(10, )"]
POSITION: line=4, character=18
EXPECTED:
- height:
- title:
===
