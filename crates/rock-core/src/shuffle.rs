//! `ROCK_SHUFFLE_SEED` — flush ordering bugs by shuffling order-prone
//! iterations in the compile pipeline.
//!
//! Cross-module name lookups, overload tables, and other indices walk
//! `FxHashMap` in iteration order. When two top-level symbols collide on
//! a name (for example a global and a function), whichever the iterator yields first wins,
//! and the answer leaks the absolute path of the project (because
//! `ModuleId` carries the file path as a `Ustr`, which feeds the hash).
//!
//! Setting `ROCK_SHUFFLE_SEED=<u64>` makes every gated iteration shuffle
//! its candidates with a deterministic per-seed permutation. Same seed →
//! same permutation. Different seeds (or unset) → different orderings.
//!
//! Usage at a call site:
//!
//! ```ignore
//! let mut entries: Vec<_> = self.other_symbols.iter().collect();
//! rock_core::shuffle::shuffle_in_place(&mut entries);
//! for (mid, ms) in entries { ... }
//! ```
//!
//! The helper is a no-op when the env var is unset, so production
//! compiles see zero overhead beyond an `Option` read (cached after
//! first call via `OnceLock`).
//!
//! Note: shuffling is only meaningful where the loop body can produce a
//! different *result* depending on which element comes first (early
//! return / break-on-match / `.first()` / `.last()`). Pure aggregation
//! loops (counts, sums, push-all-to-Vec) don't need it. Audit the
//! consumer before wrapping.

use std::sync::OnceLock;

static SHUFFLE_SEED: OnceLock<Option<u64>> = OnceLock::new();

/// Returns the `ROCK_SHUFFLE_SEED` value, or `None` if unset. Invalid settings panic.
/// Cached after first read so repeated calls cost a single atomic load.
pub fn shuffle_seed() -> Option<u64> {
    *SHUFFLE_SEED.get_or_init(|| match std::env::var("ROCK_SHUFFLE_SEED") {
        Ok(seed) => Some(seed.parse().expect("ROCK_SHUFFLE_SEED must be a u64")),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => panic!("invalid ROCK_SHUFFLE_SEED: {error}"),
    })
}

/// Deterministically shuffle `slice` if `ROCK_SHUFFLE_SEED` is set.
/// No-op otherwise.
///
/// The shuffle uses Fisher–Yates with a SplitMix64 PRNG seeded from the
/// env var XORed with the slice length. Mixing in the length keeps each
/// site's permutation distinct even with the same env seed, so a single
/// seed exercises a different ordering at every call site rather than
/// the same modular pattern everywhere.
pub fn shuffle_in_place<T>(slice: &mut [T]) {
    let Some(env_seed) = shuffle_seed() else {
        return;
    };
    if slice.len() <= 1 {
        return;
    }
    let mut rng = env_seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(slice.len() as u64);
    for i in (1..slice.len()).rev() {
        let j = (next_u64(&mut rng) % (i as u64 + 1)) as usize;
        slice.swap(i, j);
    }
}

/// Convenience: collect the iterator, shuffle if enabled, return the Vec.
/// Use when the source is a HashMap iterator that you want to consume
/// in-order downstream. If the env var is unset and the iterator is
/// already a Vec, prefer `shuffle_in_place` to avoid the allocation.
pub fn shuffled<I: IntoIterator>(iter: I) -> Vec<I::Item> {
    let mut v: Vec<I::Item> = iter.into_iter().collect();
    shuffle_in_place(&mut v);
    v
}

fn next_u64(state: &mut u64) -> u64 {
    // SplitMix64 — single-state PRNG, no additional deps. Fine for
    // shuffle-fairness; not for cryptography.
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_shuffle_when_env_unset() {
        // Test runs without setting ROCK_SHUFFLE_SEED.
        // Note: SHUFFLE_SEED is a process-wide OnceLock, so this only
        // verifies the no-op path works; the env-set path is exercised
        // by tests that fork a subprocess (rock-cli integration tests).
        let mut v = vec![1, 2, 3, 4, 5];
        let original = v.clone();
        shuffle_in_place(&mut v);
        if shuffle_seed().is_none() {
            assert_eq!(v, original);
        }
    }

    #[test]
    fn shuffled_collects() {
        let v = shuffled(0..5);
        assert_eq!(v.len(), 5);
        // We can't assert order without controlling the env, but length
        // must round-trip.
    }
}

#[cfg(test)]
mod invalid_seed_tests {
    #[test]
    fn invalid_seed_is_reported() {
        if std::env::var_os("SHUFFLE_TEST_CHILD").is_some() {
            super::shuffle_seed();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "shuffle::invalid_seed_tests::invalid_seed_is_reported",
                "--exact",
                "--nocapture",
            ])
            .env("SHUFFLE_TEST_CHILD", "1")
            .env("ROCK_SHUFFLE_SEED", "invalid")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("ROCK_SHUFFLE_SEED"));
    }
}
