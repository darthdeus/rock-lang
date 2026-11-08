# Coroutines

A `co fn` creates a resumable computation. Calling it creates a handle; the
body runs when the handle is resumed. `yield` suspends the body and produces a
value for the caller. A later resume continues after that yield.

```rock
co fn counter() -> Coroutine<int, string> {
    yield 1
    yield 2
    return "done"
}

fn main() {
    let c = counter()
    print(c.resume() ?? -1) // 1
    print(c.resume() ?? -1) // 2
    let _ = c.resume()      // completes
    print(c.completed())    // true
    print(c.result() ?? "no result") // done
}
```

`Coroutine<Y, R, N>` names the yield, return, and resume-argument types.
`R` and `N` default to `()`, so `Coroutine<Y>` means
`Coroutine<Y, (), ()>`. `resume` returns `Y?`; `result()` returns `R?`.
A nullable yield can itself be `null`, so use `status()` rather than the
resume result alone to tell whether the coroutine is still suspended.

## Resume arguments and delegation

When `N` is not `()`, call `resume` with an argument of type `N`. The value of
`yield` as an expression is the argument passed on the *next* resume. The
argument to the first resume is discarded because no yield is waiting yet.

```rock
co fn exchange() -> Coroutine<int, int, int> {
    let answer = yield 1
    return answer
}

fn main() {
    let c = exchange()
    print(c.resume(0) ?? -1) // 1; the first argument is discarded
    let _ = c.resume(42)
    print(c.result() ?? -1) // 42
}
```

`await inner()` delegates to another coroutine: its yields pass through the
outer coroutine and the `await` expression evaluates to the inner return value.
The yield types must match, and the inner coroutine must accept the outer
coroutine's resume arguments. An error in the inner coroutine propagates to the
outer one. Coroutines with unit resume arguments can also be iterated with
`for`; the iterator handles nullable yields without treating them as the end.

## Completion and cleanup

`status()` returns `"suspended"`, `"dead"`, or `"errored"`. Natural completion
and cancellation both report `"dead"`; `completed()` distinguishes natural
completion from cancellation and error. `result()` supplies a value after a
value-bearing return; it is null while suspended or after cancellation or
error. A unit or nullable return can also yield a null result, so check
`completed()` when the distinction matters. `last_error()` returns the captured
message for an errored coroutine. Later resumes of a terminal coroutine return
null.

`kill()` cancels a live coroutine and is idempotent. Pending `defer` blocks run
on normal scope exit and on termination, including cancellation and error.
Cancellation propagates into an active `await` chain. During terminal cleanup,
defers run in reverse order; an error in one is logged and does not prevent the
remaining terminal defers from running.

A coroutine may declare `on_resume`, `on_killed`, and `on_hot_reload` hook
blocks. `on_resume` runs at the start of each resume, including the first.
`on_killed` runs once on cancellation or error. `on_hot_reload` runs when
`fire_hot_reload` is called for the live coroutine; ordinary resume does not
invoke it. Hooks are declared inside the coroutine body.
