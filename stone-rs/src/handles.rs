//! The id space every native handle class draws from, and the two seams that
//! move an id into and out of a class's live set.
//!
//! `native_ledger` publishes live handle SETS and is structurally blind to a
//! registry it was never told about: the set is empty for a class nobody asks
//! for. `Class` closes half of that -- it sizes
//! `CLASSES`, so a variant without a ledger entry, or a ledger entry without a
//! variant, is a compile error. This module closes the other half. Every id a
//! covered class hands out comes from `mint` and leaves through `retire`, so
//! the ledger can ask a question no inventory can answer for itself: are the
//! ids this class ISSUED all accounted for by the live set it publishes?
//!
//! That question is what catches a registry whose `live` fn is wrong. A class
//! that mints through the seam and then under-reports its live set has issued
//! more ids than it can account for, and `native_ledger::id_accounting` says so
//! -- where the set diff would happily compare two equally-wrong answers.
//!
//! Per-class counters, not one global one. Ids are keys into per-class maps and
//! several classes' handles reach Rock and its goldens, so the numbering is
//! preserved exactly: each class counts from 1, and 0 is never minted (callers
//! throughout the crate use 0 as "no handle").

use std::sync::atomic::{AtomicU64, Ordering};

/// One variant per `native_ledger::CLASSES` entry, in the same order.
///
/// The discriminant IS the index into `CLASSES` and into the counter arrays
/// below, pinned by a const assertion at the ledger.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Class {
    Font,
    Shader,
    Mesh,
    Canvas,
    ImageData,
    Image,
    Sound,
    Voice,
    Music,
    PixelReadback,
    Cursor,
}

impl Class {
    pub(crate) const COUNT: usize = Class::Cursor as usize + 1;

    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// Whether this class's ids come from `mint`.
    ///
    /// Fonts do not, and cannot: a font id is its 1-based slot in the `FONTS`
    /// vec, so the vec IS the allocator and the id carries an index the raster
    /// and atlas paths dereference. Handing it a number from a counter would
    /// index the wrong slot. `stone_proc` is the same shape one step further --
    /// its handles are RECYCLED slots -- which is why it is not a ledger class
    /// at all and has its own occupancy-driven teardown.
    ///
    /// A false here means id accounting cannot see the class; it does NOT mean
    /// the identity diff cannot. Fonts are covered on both ledger axes.
    ///
    /// Spelled as an exhaustive match rather than a `matches!` negation because
    /// it is the second half of the completeness mechanism. `COUNT` is derived
    /// from the LAST variant, so a variant inserted anywhere before it grows the
    /// count and `CLASSES` fails to typecheck at its declared length -- but a
    /// variant appended AFTER `Cursor` would leave the count alone. This
    /// match is what refuses that one: a new variant is a non-exhaustive-match
    /// error here, whichever end it was added at.
    pub(crate) const fn mints(self) -> bool {
        match self {
            Class::Font => false,
            Class::Shader
            | Class::Mesh
            | Class::Canvas
            | Class::ImageData
            | Class::Image
            | Class::Sound
            | Class::Voice
            | Class::Music
            | Class::PixelReadback
            | Class::Cursor => true,
        }
    }
}

static ISSUED: [AtomicU64; Class::COUNT] = [const { AtomicU64::new(0) }; Class::COUNT];
static RETIRED: [AtomicU64; Class::COUNT] = [const { AtomicU64::new(0) }; Class::COUNT];

/// Hand out `class`'s next id. Monotonic per class and never 0.
///
/// The one place a covered class's handles come from. A registry that mints its
/// own counter beside this one still gets working ids, and the identity diff
/// still compares its sets -- but it has bypassed the accounting, and that is
/// exactly what `native_ledger::id_accounting` reports as live handles this
/// class never issued.
pub(crate) fn mint(class: Class) -> u64 {
    debug_assert!(class.mints(), "{class:?} does not mint through the seam");
    ISSUED[class.index()].fetch_add(1, Ordering::SeqCst) + 1
}

/// Record that `id` has left `class`'s live set.
///
/// Called where the registry entry is actually REMOVED, and only when the
/// removal happened -- a retire on a miss would credit a destroy that did not
/// occur, which is the accounting version of a teardown reporting success from
/// the destroy call rather than from the live set.
pub(crate) fn retire(class: Class, id: u64) {
    debug_assert!(id != 0, "{class:?}: retired the never-minted id 0");
    RETIRED[class.index()].fetch_add(1, Ordering::SeqCst);
}

/// How many ids `class` has issued; equivalently its highest live-or-dead id,
/// since minting is monotonic from 1.
pub(crate) fn issued(class: Class) -> u64 {
    ISSUED[class.index()].load(Ordering::SeqCst)
}

/// How many of `class`'s ids have been recorded as leaving its live set.
pub(crate) fn retired(class: Class) -> u64 {
    RETIRED[class.index()].load(Ordering::SeqCst)
}
