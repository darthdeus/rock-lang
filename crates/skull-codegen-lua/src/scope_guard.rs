//! Check the scope of emitted cell names around counted loops.
//! A mismatch is valid Lua that reads the wrong variable.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once any render installs a guard, so the per-name hooks cost one
/// relaxed load while checks are off.
static ANY: AtomicBool = AtomicBool::new(false);

struct Guard {
    func: *const MirFunc,
    /// `(header, counter cell, counter's printed number)` per counted loop.
    loops: Vec<(Idx<Block>, Idx<Cell>, u32)>,
    open: Vec<Idx<Block>>,
    writing: bool,
}

thread_local! {
    static GUARDS: RefCell<Vec<Guard>> = const { RefCell::new(Vec::new()) };
}

/// Removes the guard [`install`] pushed when the render ends.
pub(crate) struct Installed(bool);

impl Drop for Installed {
    fn drop(&mut self) {
        if self.0 {
            GUARDS.with(|g| g.borrow_mut().pop());
        }
    }
}

pub(crate) fn install(
    func: &MirFunc,
    loops: &HashMap<Idx<Block>, ForLoop>,
    cell_colors: &[u32],
) -> Installed {
    if !skull_mir::verify::scope_checks_enabled() {
        return Installed(false);
    }
    ANY.store(true, Ordering::Relaxed);
    let loops = loops
        .iter()
        .filter_map(|(&h, cl)| match cl.control {
            ForControl::Numeric { slot, .. } => {
                Some((h, slot, cell_number(func, cell_colors, slot)))
            }
            ForControl::Map { .. } => None,
        })
        .collect();
    GUARDS.with(|g| {
        g.borrow_mut().push(Guard {
            func,
            loops,
            open: Vec::new(),
            writing: false,
        })
    });
    Installed(true)
}

fn cell_number(func: &MirFunc, colors: &[u32], c: Idx<Cell>) -> u32 {
    match colors[raw(c) as usize] {
        u32::MAX => func.emit_cell_id(c),
        col => col,
    }
}

fn with_guard(func: &MirFunc, f: impl FnOnce(&mut Guard)) {
    if !ANY.load(Ordering::Relaxed) {
        return;
    }
    GUARDS.with(|g| {
        if let Some(guard) = g.borrow_mut().last_mut()
            && std::ptr::eq(guard.func, func)
        {
            f(guard);
        }
    });
}

/// The `for` line of `header` starts printing: its counter is in scope until
/// [`close`].
pub(crate) fn open(func: &MirFunc, header: Idx<Block>) {
    with_guard(func, |g| g.open.push(header));
}

pub(crate) fn close(func: &MirFunc, header: Idx<Block>) {
    with_guard(func, |g| {
        let top = g.open.pop();
        assert_eq!(top, Some(header), "counted loops must close in order");
    });
}

/// The next cell name printed is assigned to. Outside its loop a counter may
/// be assigned (the preheader's initial value goes into the outer local the
/// `for` line reads) but never read.
pub(crate) fn assigning(func: &MirFunc) {
    with_guard(func, |g| g.writing = true);
}

pub(crate) fn cell(func: &MirFunc, c: Idx<Cell>, number: u32) {
    with_guard(func, |g| {
        let writing = std::mem::take(&mut g.writing);
        for &(h, slot, n) in &g.loops {
            let open = g.open.contains(&h);
            if slot == c {
                assert!(
                    open || writing,
                    "loop counter c{n} printed outside its `for` (header bb{})",
                    raw(h)
                );
            } else if n == number && open {
                panic!(
                    "c{number} printed inside the `for` (header bb{}) whose counter shadows it",
                    raw(h)
                );
            }
        }
    });
}

/// `c<number> = nil` is being printed: inside a loop that kills its counter.
pub(crate) fn cell_kill(func: &MirFunc, number: u32) {
    with_guard(func, |g| {
        for &(h, _, n) in &g.loops {
            assert!(
                n != number || !g.open.contains(&h),
                "kill of c{number} inside the `for` (header bb{}) whose counter it names",
                raw(h)
            );
        }
    });
}
