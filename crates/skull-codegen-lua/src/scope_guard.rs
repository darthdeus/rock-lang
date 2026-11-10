//! A compiler-bug check on printed names: every cell or register name must be
//! printed where the Lua local it means is in scope. A counted loop's counter
//! is a fresh `for` local, so outside its loop the same name is the outer
//! local, and a cell sharing that name is shadowed inside the loop. A
//! loop-owned register is declared inside its `for` block, so outside it the
//! name is a global. Either mismatch prints valid Lua that reads the wrong
//! variable, so it is checked here, at print time, when MIR checks are on.

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
    /// The next cell name printed is an assignment target.
    writing: bool,
    color_scope: Vec<Option<Idx<Block>>>,
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
    loops: &HashMap<Idx<Block>, CountedLoop>,
    cell_colors: &[u32],
    color_scope: &[Option<Idx<Block>>],
) -> Installed {
    if !skull_mir::verify::checks_enabled() {
        return Installed(false);
    }
    ANY.store(true, Ordering::Relaxed);
    let loops = loops
        .iter()
        .map(|(&h, cl)| (h, cl.slot, cell_number(func, cell_colors, cl.slot)))
        .collect();
    GUARDS.with(|g| {
        g.borrow_mut().push(Guard {
            func,
            loops,
            open: Vec::new(),
            writing: false,
            color_scope: color_scope.to_vec(),
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

/// A cell's local name, `c<number>`, is being printed.
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

/// A register's name, `r<color>`, is being printed.
pub(crate) fn register(func: &MirFunc, color: u32) {
    with_guard(func, |g| {
        if let Some(Some(h)) = g.color_scope.get(color as usize) {
            assert!(
                g.open.contains(h),
                "loop-owned register r{color} printed outside its `for` (header bb{})",
                raw(*h)
            );
        }
    });
}
