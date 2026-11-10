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

/// `ustr` of `parts` joined, spelled into a stack buffer: these names are
/// minted per cell and per field, where a `format!` is one allocation each.
pub fn concat(parts: &[&str]) -> Ustr {
    let mut buf = [0u8; 96];
    let len: usize = parts.iter().map(|part| part.len()).sum();
    if len > buf.len() {
        return ustr(&parts.concat());
    }
    let mut at = 0;
    for part in parts {
        buf[at..at + part.len()].copy_from_slice(part.as_bytes());
        at += part.len();
    }
    // Whole `str`s end to end are valid UTF-8.
    ustr(std::str::from_utf8(&buf[..len]).expect("joined names"))
}

/// `ustr` of `"{base}{sep}{index}"`.
pub fn indexed(base: &str, sep: &str, index: usize) -> Ustr {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut rest = index;
    loop {
        at -= 1;
        digits[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let digits = std::str::from_utf8(&digits[at..]).expect("decimal digits");
    concat(&[base, sep, digits])
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

    #[test]
    fn joined_names_match_formatting() {
        let long = "n".repeat(200);
        for base in ["", "self", long.as_str()] {
            for index in [0, 7, 10, 4096, usize::MAX] {
                assert_eq!(
                    super::indexed(base, "__", index).as_str(),
                    format!("{base}__{index}")
                );
            }
            assert_eq!(
                super::concat(&[base, ".", "x"]).as_str(),
                format!("{base}.x")
            );
        }
        assert_eq!(super::concat(&[]).as_str(), "");
    }
}
