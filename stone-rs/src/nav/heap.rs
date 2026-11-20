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

/// Dijkstra over a small tile window: stamped distances and parents, and an
/// unordered open list scanned for the cheapest (cost, tile) on each pop.
#[derive(Clone, Debug, Default)]
pub struct WindowSearch {
    pub stamp: int,
    pub dist: IdxVec<int>,
    pub seen: IdxVec<int>,
    pub parent: IdxVec<int>,
    open_cost: IdxVec<int>,
    open_tile: IdxVec<int>,
    open_slot: IdxVec<int>,
    count: int,
}

impl WindowSearch {
    pub fn reserve(&mut self, tiles: int) {
        if self.dist.len() < tiles {
            let n = tiles as usize;
            for v in [
                &mut self.dist,
                &mut self.seen,
                &mut self.open_cost,
                &mut self.open_tile,
            ] {
                v.0.resize(n, 0);
            }
            self.parent.0.resize(n, -1);
            self.open_slot.0.resize(n, -1);
        }
    }

    pub fn begin(&mut self) {
        self.stamp += 1;
        self.count = 0;
    }

    pub fn reached(&self, t: int) -> bool {
        self.seen[t] == self.stamp
    }

    pub fn seed(&mut self, t: int) {
        self.dist[t] = 0;
        self.seen[t] = self.stamp;
        self.parent[t] = -1;
        self.open(t, 0);
    }

    fn open(&mut self, t: int, cost: int) {
        self.open_cost[self.count] = cost;
        self.open_tile[self.count] = t;
        self.open_slot[t] = self.count;
        self.count += 1;
    }

    pub fn relax(&mut self, from: int, t: int, cost: int) {
        if !self.reached(t) {
            self.dist[t] = cost;
            self.seen[t] = self.stamp;
            self.parent[t] = from;
            self.open(t, cost);
        } else if cost < self.dist[t] {
            self.dist[t] = cost;
            self.parent[t] = from;
            self.open_cost[self.open_slot[t]] = cost;
        }
    }

    /// Removes the cheapest open tile, the lowest index among equal costs.
    pub fn pop(&mut self) -> Option<(int, int)> {
        if self.count == 0 {
            return None;
        }
        let mut best = 0;
        let mut d = self.open_cost[0];
        let mut t = self.open_tile[0];
        for e in 1..self.count {
            let (ec, et) = (self.open_cost[e], self.open_tile[e]);
            if ec < d || (ec == d && et < t) {
                best = e;
                d = ec;
                t = et;
            }
        }
        self.count -= 1;
        let moved = self.open_tile[self.count];
        self.open_cost[best] = self.open_cost[self.count];
        self.open_tile[best] = moved;
        self.open_slot[moved] = best;
        self.open_slot[t] = -1;
        Some((t, d))
    }
}
