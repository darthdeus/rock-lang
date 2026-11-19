use std::sync::atomic::{AtomicU64, Ordering};

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

pub(crate) fn mint(class: Class) -> u64 {
    debug_assert!(class.mints(), "{class:?} does not mint through the seam");
    ISSUED[class.index()].fetch_add(1, Ordering::SeqCst) + 1
}

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
