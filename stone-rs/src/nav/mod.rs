//! MAG's integer navigation graph and route planner.
//!
//! Graph revisions are immutable while queries use them. Search scratch and
//! caches belong to a query; workers never touch Lua or published plan storage.
#![allow(non_camel_case_types)]
use std::collections::BTreeMap;
use std::ops::{Index, IndexMut};
type int = i64;
const SPACE_SIZE: int = 120;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdxVec<T>(pub Vec<T>, int);
impl<T> IdxVec<T> {
    pub fn capacity(&self) -> int {
        self.1
    }
    pub fn len(&self) -> int {
        self.0.len() as int
    }
    pub fn push(&mut self, item: T) {
        if self.len() == self.1 {
            self.grow();
        }
        self.0.push(item);
    }
    pub fn pop(&mut self) -> Option<T> {
        self.0.pop()
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }
    pub fn reserve(&mut self, capacity: int) {
        if capacity > self.1 {
            self.0.reserve_exact(capacity as usize - self.0.len());
            self.1 = capacity;
        }
    }
    pub fn grow(&mut self) {
        self.reserve(if self.1 < 2 { 4 } else { self.1 * 2 });
    }
}
impl<T> Index<int> for IdxVec<T> {
    type Output = T;
    fn index(&self, index: int) -> &T {
        &self.0[usize::try_from(index).expect("negative nav index")]
    }
}
impl<T> IndexMut<int> for IdxVec<T> {
    fn index_mut(&mut self, index: int) -> &mut T {
        &mut self.0[usize::try_from(index).expect("negative nav index")]
    }
}
impl<T> From<Vec<T>> for IdxVec<T> {
    fn from(v: Vec<T>) -> Self {
        let capacity = v.len() as int;
        Self(v, capacity)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IVec2 {
    pub x: int,
    pub y: int,
}
fn iv(x: int, y: int) -> IVec2 {
    IVec2 { x, y }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IRect {
    pub x: int,
    pub y: int,
    pub w: int,
    pub h: int,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceId {
    pub value: int,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Place {
    pub space: SpaceId,
    pub grid: IVec2,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct NavSpaceLayout {
    pub space: SpaceId,
    pub origin: IVec2,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct NavLinkInput {
    pub from: Place,
    pub to: Place,
    pub cost: int,
    pub requirement: int,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NavPlanId {
    pub slot: int,
    pub generation: int,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NavRouteTile {
    pub x: int,
    pub y: int,
    pub link: int,
}
#[derive(Clone, Copy, Debug)]
pub struct NavRegion {
    pub space: SpaceId,
    pub rect: IRect,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavStep {
    Pending,
    Step(Place),
    Teleport(Place),
    Reached,
    Unreachable,
    Blocked,
}
use std::cmp::{max, min};
fn diagonal_elbow_clear(blocked: bool, corners: int, corner: int) -> bool {
    !blocked || (corners & (1 << corner)) != 0
}
fn diagonal_step_corner(sx: int, sy: int, x_elbow: bool) -> int {
    let c = if sx > 0 {
        if sy > 0 {
            3
        } else {
            0
        }
    } else if sy > 0 {
        2
    } else {
        1
    };
    if x_elbow {
        c
    } else {
        (c + 2) % 4
    }
}
#[inline(never)]
fn lua_tobit(value: f64) -> u32 {
    // LuaJIT's numeric ToBit conversion. Keep this rounding boundary separate
    // from the caller's products, including under arm64 multiply-add lowering.
    (value + 6755399441055744.0).to_bits() as u32
}
fn route_hash(seed: int, x: int, y: int) -> int {
    let mut h = lua_tobit(seed as f64)
        ^ lua_tobit(x as f64 * 73856093.0)
        ^ lua_tobit(y as f64 * 19349663.0);
    h ^= h << 13;
    h ^= h >> 17;
    h ^= h << 5;
    (h & 0x7fffffff) as int
}
mod types;
use types::*;
mod graph;
use graph::*;
mod heap;
use heap::*;
mod areas;
use areas::*;
mod search;
use search::*;
mod geometry;
use geometry::*;
mod query;
use query::*;

mod descending;
use descending::*;
mod jobs;
use jobs::*;
mod ffi;

#[cfg(test)]
mod tests;

pub(super) fn shutdown() {
    ffi::shutdown();
    nav_jobs_shutdown();
}
