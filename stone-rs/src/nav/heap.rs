use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct HeapEntry {
    pub f: int,
    pub g: int,
    pub n: int,
}

impl HeapEntry {
    fn before(&self, other: &Self) -> bool {
        self.f < other.f
            || (self.f == other.f && (self.g > other.g || (self.g == other.g && self.n < other.n)))
    }
}

#[derive(Clone, Debug, Default)]
pub struct NavHeap(Vec<HeapEntry>);

impl NavHeap {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn push(&mut self, f: int, g: int, n: int) {
        let item = HeapEntry { f, g, n };
        let h = &mut self.0;
        let mut i = h.len();
        h.push(item);
        while i > 0 {
            let parent = (i - 1) / 2;
            if h[parent].before(&item) {
                break;
            }
            h[i] = h[parent];
            i = parent;
        }
        h[i] = item;
    }

    pub fn pop(&mut self) -> Option<HeapEntry> {
        let top = *self.0.first()?;
        let item = self.0.pop()?;
        let h = &mut self.0;
        if h.is_empty() {
            return Some(top);
        }
        let mut i = 0;
        loop {
            let mut child = 2 * i + 1;
            if child >= h.len() {
                break;
            }
            if child + 1 < h.len() && h[child + 1].before(&h[child]) {
                child += 1;
            }
            if !h[child].before(&item) {
                break;
            }
            h[i] = h[child];
            i = child;
        }
        h[i] = item;
        Some(top)
    }
}
