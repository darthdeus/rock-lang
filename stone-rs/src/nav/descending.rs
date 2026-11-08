use super::*;

#[derive(Clone, Copy)]
pub(super) struct DescendCandidate {
    pub place: Place,
    x: int,
    y: int,
    distance: int,
    hop: int,
}
#[derive(Clone)]
pub(super) struct DescendSelection {
    pub candidates: Vec<DescendCandidate>,
    seed: int,
    here: int,
    best: int,
    rank: int,
    pub chosen: Option<Place>,
}
impl DescendSelection {
    pub fn candidate(&self, index: usize, phase: int) -> Option<Place> {
        let c = self.candidates.get(index)?;
        if phase == 0 {
            if c.distance < self.best {
                Some(c.place)
            } else {
                None
            }
        } else if self.candidate_rank(c) > self.rank {
            Some(c.place)
        } else {
            None
        }
    }
    fn candidate_rank(&self, c: &DescendCandidate) -> int {
        // Distances are integers. This is the exact admission boundary of
        // distance <= best + 0.2*hop, without floating point or contraction.
        if c.distance >= self.here || c.distance * 5 > self.best * 5 + c.hop {
            -1
        } else {
            route_hash(self.seed, c.x, c.y)
        }
    }
    pub fn accept(&mut self, index: usize, phase: int) {
        let c = self.candidates[index];
        if phase == 0 {
            self.best = c.distance;
        } else {
            self.rank = self.candidate_rank(&c);
            self.chosen = Some(c.place);
        }
    }
}
pub(super) fn nav_descend_prepare(
    g: &NavGraph,
    q: &mut NavQuery,
    plan: &NavPlan,
    at: Place,
) -> Option<DescendSelection> {
    if plan.route.len() < 2 {
        return None;
    }
    let si = nav_space_index(g, at.space);
    let wx = g.space_ox[si] + at.grid.x;
    let wy = g.space_oy[si] + at.grid.y;
    let ca = nav_tile_cluster(g, si, at.grid.x, at.grid.y);
    let mut last = min(plan.path_pos + 1, plan.route.len() - 1);
    let mut cb = ca;
    for p in (plan.path_pos + 1)..plan.route.len() {
        if plan.route[p].link != 0 {
            break;
        }
        let ps = nav_world_space(g, plan.route[p].x, plan.route[p].y, si);
        if ps < 0 {
            break;
        }
        let pc = nav_tile_cluster(
            g,
            ps,
            plan.route[p].x - g.space_ox[ps],
            plan.route[p].y - g.space_oy[ps],
        );
        if pc != ca && cb != ca && pc != cb {
            break;
        }
        if !nav_clusters_near(g, ca, pc) {
            break;
        }
        cb = pc;
        last = p;
    }
    let tx = plan.route[last].x;
    let ty = plan.route[last].y;
    if nav_world_space(g, tx, ty, si) < 0 || !nav_clusters_near(g, ca, cb) {
        return None;
    }
    nav_tile_search(
        g,
        q,
        ca,
        cb,
        IRect {
            x: tx,
            y: ty,
            w: 1,
            h: 1,
        },
        false,
        IRect {
            x: wx,
            y: wy,
            w: 1,
            h: 1,
        },
        true,
    );
    if q.tile_found < 0 {
        return None;
    }
    let here = q.tile_dist[q.tile_found];
    let mut candidates = Vec::new();
    for dy in -1..2 {
        for dx in -1..2 {
            if dx == 0 && dy == 0 {
                continue;
            }
            let nx = wx + dx;
            let ny = wy + dy;
            if nav_window_tile(g, ca, cb, nx, ny) < 0 {
                continue;
            }
            let t = (ny - q.tile_oy) * q.tile_stride + nx - q.tile_ox;
            if q.tile_stamp[t] != q.tile_stamp_id || q.tile_dist[t] >= here {
                continue;
            }
            if !nav_world_step_open(g, wx, wy, nx, ny, si) {
                continue;
            }
            let ns = nav_world_space(g, nx, ny, si);
            if ns < 0 {
                continue;
            }
            candidates.push(DescendCandidate {
                place: Place {
                    space: SpaceId {
                        value: g.space_id[ns],
                    },
                    grid: iv(nx - g.space_ox[ns], ny - g.space_oy[ns]),
                },
                x: nx,
                y: ny,
                distance: q.tile_dist[t],
                hop: if dx != 0 && dy != 0 {
                    NAV_COST_DIAGONAL
                } else {
                    NAV_COST_STRAIGHT
                },
            });
        }
    }
    Some(DescendSelection {
        candidates,
        seed: plan.seed,
        here,
        best: here,
        rank: -1,
        chosen: None,
    })
}
