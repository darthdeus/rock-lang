//! Interning for names the backend spells again and again.
//!
//! `Ustr::from` takes one of 64 global locks chosen by the string's hash, so
//! every worker spelling the same short name (`""`, `"0"`, `"self__0"`, a
//! leaf path) queues on the same lock. Each thread remembers the names it
//! has interned and only takes the lock for a name it has not seen.

use rustc_hash::FxHashMap;
use std::cell::RefCell;
use ustr::Ustr;

thread_local! {
    static INTERNED: RefCell<FxHashMap<Box<str>, Ustr>> = RefCell::default();
}

/// The same value as `Ustr::from(name)`.
pub fn ustr(name: &str) -> Ustr {
    INTERNED.with(|interned| {
        if let Some(found) = interned.borrow().get(name) {
            return *found;
        }
        let fresh = Ustr::from(name);
        interned.borrow_mut().insert(name.into(), fresh);
        fresh
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn matches_global_interning() {
        for name in ["", "0", "self__0", "a.b"] {
            assert_eq!(super::ustr(name), ustr::Ustr::from(name));
            assert_eq!(super::ustr(name), ustr::Ustr::from(name));
        }
    }
}
