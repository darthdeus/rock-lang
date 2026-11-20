use super::*;

pub(super) fn nav_step_to(g: &NavGraph, si: int, wx: int, wy: int) -> NavStep {
    let ts = nav_world_space(g, wx, wy, si);
    NavStep::Step(nav_world_place(g, ts, wx, wy))
}

pub(super) fn nav_clusters_near(g: &NavGraph, a: int, b: int) -> bool {
    let ax = g.cluster_wx[a];
    let ay = g.cluster_wy[a];
    let bx = g.cluster_wx[b];
    let by = g.cluster_wy[b];
    let s = g.cluster_size;
    ax <= bx + s && bx <= ax + s && ay <= by + s && by <= ay + s
}

pub(super) fn nav_refine(
    g: &NavGraph,
    q: &mut NavQuery,
    plan: &mut NavPlan,
    si: int,
    x: int,
    y: int,
) -> NavStep {
    q.refine_work = 0;
    let ca = nav_tile_cluster(g, si, x, y);
    let wx = g.space_ox[si] + x;
    let wy = g.space_oy[si] + y;
    loop {
        let mut w = plan.wp_next;
        if w < plan.wp.len() && plan.wp[w].link != 0 {
            if wx == plan.wp[w].x && wy == plan.wp[w].y {
                plan.wp_next += 1;
                continue;
            }
            if wx == plan.wp[w - 1].x && wy == plan.wp[w - 1].y {
                plan.seg.clear();
                return NavStep::Teleport(place(g.space_id[si], x, y));
            }
            plan.wp_next -= 1;
            continue;
        }
        let mut home = plan.final_cluster;
        let mut target = rect(plan.goal_wx, plan.goal_wy, plan.goal_w, plan.goal_h);
        let seed = rect(wx, wy, 1, 1);
        let mut direct = false;
        if w < plan.wp.len() && w > plan.last_link && nav_clusters_near(g, ca, home) {
            q.refine_work += nav_tile_search(g, q, ca, home, seed, true, target, true);
            direct = q.tile_found >= 0;
        }
        if direct {
            plan.wp_next = plan.wp.len();
            w = plan.wp.len();
        } else {
            if w < plan.wp.len() {
                home = plan.wp[w].cluster;
                target = rect(plan.wp[w].x, plan.wp[w].y, 1, 1);
            }
            if !nav_clusters_near(g, ca, home) {
                return nav_requery(g, q, plan, si, x, y);
            }
            q.refine_work += nav_tile_search(g, q, ca, home, seed, true, target, true);
        }
        if q.tile_found < 0 {
            if nav_cluster_pending(g, ca) || nav_cluster_pending(g, home) {
                return NavStep::Blocked;
            }
            return nav_requery(g, q, plan, si, x, y);
        }
        plan.seg.clear();
        let mut t = q.tile_found;
        while t >= 0 {
            plan.seg.push(iv(
                q.tile_ox + t % q.tile_stride,
                q.tile_oy + t / q.tile_stride,
            ));
            t = q.window.parent[t];
        }
        plan.seg.0.reverse();
        if plan.seg.len() > 1 {
            return nav_step_to(g, si, plan.seg[1].x, plan.seg[1].y);
        }
        if w >= plan.wp.len() {
            return NavStep::Reached;
        }
        plan.wp_next += 1;
    }
}

pub(super) fn nav_window_tile(g: &NavGraph, c1: int, c2: int, x: int, y: int) -> int {
    let c = if x >= g.cluster_wx[c1]
        && y >= g.cluster_wy[c1]
        && x < g.cluster_wx[c1] + g.cluster_size
        && y < g.cluster_wy[c1] + g.cluster_size
    {
        c1
    } else if x >= g.cluster_wx[c2]
        && y >= g.cluster_wy[c2]
        && x < g.cluster_wx[c2] + g.cluster_size
        && y < g.cluster_wy[c2] + g.cluster_size
    {
        c2
    } else {
        return -1;
    };
    let si = g.cluster_space[c];
    nav_tile_index(g, si, x - g.space_ox[si], y - g.space_oy[si])
}

pub(super) fn nav_elbow_clear(g: &NavGraph, c1: int, c2: int, x: int, y: int, corner: int) -> bool {
    let t = nav_window_tile(g, c1, c2, x, y);
    if t >= 0 {
        return diagonal_elbow_clear(g.blocked[t], g.corners[t], corner);
    }
    nav_world_elbow_clear(g, x, y, corner, g.cluster_space[c1])
}

pub(super) fn nav_tile_search(
    g: &NavGraph,
    q: &mut NavQuery,
    c1: int,
    c2: int,
    seed: IRect,
    seed_blocked_ok: bool,
    target: IRect,
    stop: bool,
) -> int {
    let stride = 2 * g.cluster_size;
    let ox = g.cluster_wx[c1].min(g.cluster_wx[c2]);
    let oy = g.cluster_wy[c1].min(g.cluster_wy[c2]);
    q.tile_ox = ox;
    q.tile_oy = oy;
    q.tile_stride = stride;
    q.tile_found = -1;
    let w = &mut q.window;
    w.begin();
    for y in seed.y..(seed.y + seed.h) {
        for x in seed.x..(seed.x + seed.w) {
            let bt = nav_window_tile(g, c1, c2, x, y);
            if bt >= 0 && (seed_blocked_ok || !g.blocked[bt]) {
                w.seed((y - oy) * stride + (x - ox));
            }
        }
    }
    let mut expanded = 0;
    while let Some((t, d)) = w.pop() {
        expanded += 1;
        let x = ox + t % stride;
        let y = oy + t / stride;
        let here_extra = g.extra[nav_window_tile(g, c1, c2, x, y)];
        if stop
            && x >= target.x
            && y >= target.y
            && x < target.x + target.w
            && y < target.y + target.h
        {
            q.tile_found = t;
            return expanded;
        }
        let mut relax = |nx: int, ny: int, step: int, bt: int| {
            let cost = d + nav_edge_cost(step, here_extra, g.extra[bt]);
            w.relax(t, (ny - oy) * stride + nx - ox, cost);
        };
        for i in 0..4 {
            let nx = x + (i % 2) * (i - 2);
            let ny = y + ((i + 1) % 2) * (i - 1);
            let bt = nav_window_tile(g, c1, c2, nx, ny);
            if bt >= 0 && !g.blocked[bt] {
                relax(nx, ny, NAV_COST_STRAIGHT, bt);
            }
        }
        for i in 0..4 {
            let dx = 1 - 2 * (((i + 1) / 2) % 2);
            let dy = 2 * (i / 2) - 1;
            let nx = x + dx;
            let ny = y + dy;
            let bt = nav_window_tile(g, c1, c2, nx, ny);
            if bt >= 0
                && !g.blocked[bt]
                && nav_elbow_clear(g, c1, c2, nx, y, diagonal_step_corner(dx, dy, true))
                && nav_elbow_clear(g, c1, c2, x, ny, diagonal_step_corner(dx, dy, false))
            {
                relax(nx, ny, NAV_COST_DIAGONAL, bt);
            }
        }
    }
    expanded
}

pub(super) fn nav_tile_dist(q: &NavQuery, x: int, y: int) -> int {
    let lx = x - q.tile_ox;
    let ly = y - q.tile_oy;
    if lx < 0 || ly < 0 || lx >= q.tile_stride || ly >= q.tile_stride {
        return -1;
    }
    let t = ly * q.tile_stride + lx;
    if !q.window.reached(t) {
        return -1;
    }
    q.window.dist[t]
}

pub(super) fn nav_hop_cost(g: &NavGraph, plan: &NavPlan, x: int, y: int, nx: int, ny: int) -> int {
    let step = if x != nx && y != ny {
        NAV_COST_DIAGONAL
    } else {
        NAV_COST_STRAIGHT
    };
    nav_edge_cost(
        step,
        nav_world_extra(g, x, y, plan.si),
        nav_world_extra(g, nx, ny, plan.si),
    )
}

pub(super) fn nav_path_push(g: &NavGraph, plan: &mut NavPlan, x: int, y: int, link: int) {
    let i = plan.route.len();
    plan.route.push(NavRouteTile { x, y, link });
    if i > 0 && link == 0 {
        let px = plan.route[i - 1].x;
        let py = plan.route[i - 1].y;
        plan.path_cost += nav_hop_cost(g, plan, px, py, x, y);
    }
}

pub(super) fn nav_route_reserve(plan: &mut NavPlan, capacity: int) {
    while plan.route.capacity() < capacity {
        plan.route.grow();
    }
}

pub(super) fn nav_route_straighten(g: &NavGraph, plan: &mut NavPlan) {
    let n = plan.route.len();
    for j in 2..n {
        let dx = plan.route[j].x - plan.route[j - 1].x;
        let dy = plan.route[j].y - plan.route[j - 1].y;
        let sx = plan.route[j - 1].x - plan.route[j - 2].x;
        let sy = plan.route[j - 1].y - plan.route[j - 2].y;
        let straight = plan.route[j - 1].link == 0 && (sx == 0 || sy == 0);
        if plan.route[j].link == 0
            && straight
            && dx != 0
            && dy != 0
            && dx.abs() == 1
            && dy.abs() == 1
            && (sx == dx || sy == dy)
        {
            let mut i = j - 1;
            while i > 1
                && plan.route[i - 1].link == 0
                && plan.route[i].x - plan.route[i - 1].x == sx
                && plan.route[i].y - plan.route[i - 1].y == sy
                && plan.route[i - 1].x - plan.route[i - 2].x == sx
                && plan.route[i - 1].y - plan.route[i - 2].y == sy
            {
                i -= 1;
            }
            let mut merges = i == 1;
            if !merges && i > 1 && plan.route[i - 1].link == 0 {
                merges = plan.route[i - 1].x - plan.route[i - 2].x == dx
                    && plan.route[i - 1].y - plan.route[i - 2].y == dy;
            }
            if merges {
                let ox = dx - sx;
                let oy = dy - sy;
                let mut ok = true;
                let mut px = plan.route[i - 1].x;
                let mut py = plan.route[i - 1].y;
                let mut old_cost = 0;
                let mut new_cost = 0;
                for k in i..(j + 1) {
                    let from = plan.route[k - 1];
                    let to = plan.route[k];
                    old_cost += nav_hop_cost(g, plan, from.x, from.y, to.x, to.y);
                }
                for k in i..j {
                    let nx = plan.route[k].x + ox;
                    let ny = plan.route[k].y + oy;
                    if !nav_world_step_open(g, px, py, nx, ny, plan.si) {
                        ok = false;
                        break;
                    }
                    new_cost += nav_hop_cost(g, plan, px, py, nx, ny);
                    px = nx;
                    py = ny;
                }
                if ok {
                    new_cost += nav_hop_cost(g, plan, px, py, plan.route[j].x, plan.route[j].y);
                }
                if ok
                    && new_cost == old_cost
                    && nav_world_step_open(g, px, py, plan.route[j].x, plan.route[j].y, plan.si)
                {
                    for k in i..j {
                        plan.route[k].x += ox;
                        plan.route[k].y += oy;
                    }
                }
            }
        }
    }
}

pub(super) fn nav_route_shortcut(g: &NavGraph, plan: &mut NavPlan) {
    let n = plan.route.len();
    if n < 3 {
        return;
    }
    let mut w = 0;
    let mut r = 1;
    while r < n {
        let mut best = r;
        let mut same = false;
        if plan.route[r].link == 0 {
            let limit = (n - 1).min(r + NAV_SHORTCUT_WINDOW);
            let mut j = r + 1;
            while j < n && plan.route[j].link == 0 {
                let dx = (plan.route[j].x - plan.route[w].x).abs();
                let dy = (plan.route[j].y - plan.route[w].y).abs();
                if dx == 0 && dy == 0 {
                    best = j;
                    same = true;
                } else if j <= limit
                    && dx <= 1
                    && dy <= 1
                    && nav_world_step_open(
                        g,
                        plan.route[w].x,
                        plan.route[w].y,
                        plan.route[j].x,
                        plan.route[j].y,
                        plan.si,
                    )
                {
                    best = j;
                    same = false;
                }
                j += 1;
            }
        }
        if best != r || same {
            let mut removed = nav_hop_cost(
                g,
                plan,
                plan.route[w].x,
                plan.route[w].y,
                plan.route[r].x,
                plan.route[r].y,
            );
            for k in (r + 1)..(best + 1) {
                removed += nav_hop_cost(
                    g,
                    plan,
                    plan.route[k - 1].x,
                    plan.route[k - 1].y,
                    plan.route[k].x,
                    plan.route[k].y,
                );
            }
            let added = if same {
                0
            } else {
                nav_hop_cost(
                    g,
                    plan,
                    plan.route[w].x,
                    plan.route[w].y,
                    plan.route[best].x,
                    plan.route[best].y,
                )
            };
            plan.path_cost += added - removed;
        }
        if !same {
            w += 1;
            plan.route[w] = plan.route[best];
        }
        r = best + 1;
    }
    while plan.route.len() > w + 1 {
        plan.route.pop();
    }
}

pub(super) fn nav_route_reverse(plan: &mut NavPlan) -> bool {
    let n = plan.route.len();
    for i in 0..n {
        if plan.route[i].link != 0 {
            return false;
        }
    }
    let mut i = 0;
    let mut j = n - 1;
    while i < j {
        let x = plan.route[i].x;
        let y = plan.route[i].y;
        plan.route[i].x = plan.route[j].x;
        plan.route[i].y = plan.route[j].y;
        plan.route[j].x = x;
        plan.route[j].y = y;
        i += 1;
        j -= 1;
    }
    true
}

pub(super) fn nav_route_finish_shape(g: &NavGraph, plan: &mut NavPlan) {
    nav_route_shortcut(g, plan);
    let flipped = route_hash(plan.seed, 1031, 7) % 2 == 1 && nav_route_reverse(plan);
    nav_route_straighten(g, plan);
    if flipped {
        nav_route_reverse(plan);
    }
}

pub(super) fn nav_reconnect(
    g: &NavGraph,
    q: &mut NavQuery,
    plan: &mut NavPlan,
    si: int,
    x: int,
    y: int,
) -> NavStep {
    let ca = nav_tile_cluster(g, si, x, y);
    if !plan.versions.iter().any(|&(c, _)| c == ca) {
        return nav_requery(g, q, plan, si, x, y);
    }
    let mut cb = ca;
    let mut target = -1;
    for p in (plan.path_pos + 1)..plan.route.len() {
        if plan.route[p].link != 0 {
            break;
        }
        let ps = nav_world_space(g, plan.route[p].x, plan.route[p].y, si);
        let pc = nav_tile_cluster(
            g,
            ps,
            plan.route[p].x - g.space_ox[ps],
            plan.route[p].y - g.space_oy[ps],
        );
        if !nav_clusters_near(g, ca, pc) || (pc != ca && cb != ca && pc != cb) {
            break;
        }
        cb = pc;
        target = p;
    }
    if target < 0 {
        return nav_requery(g, q, plan, si, x, y);
    }
    nav_tile_search(
        g,
        q,
        ca,
        cb,
        rect(g.space_ox[si] + x, g.space_oy[si] + y, 1, 1),
        true,
        rect(plan.route[target].x, plan.route[target].y, 1, 1),
        true,
    );
    if q.tile_found < 0 {
        if nav_cluster_pending(g, ca) || nav_cluster_pending(g, cb) {
            return NavStep::Blocked;
        }
        return nav_requery(g, q, plan, si, x, y);
    }
    let mut removed = 0;
    for p in (plan.path_pos + 1)..(target + 1) {
        let here = plan.route[p - 1];
        let next = plan.route[p];
        removed += nav_hop_cost(g, plan, here.x, here.y, next.x, next.y);
    }
    let remaining = plan.cost - plan.walked_cost - removed + q.window.dist[q.tile_found];
    let mut len = 0;
    let mut t = q.tile_found;
    while t >= 0 {
        len += 1;
        t = q.window.parent[t];
    }
    let tail = plan.route.len() - target - 1;
    let total = len + tail;
    if total > plan.route.capacity() {
        return nav_requery(g, q, plan, si, x, y);
    }
    if total > plan.route.len() {
        plan.route.0.resize(total as usize, NavRouteTile::default());
    }
    if len > target + 1 {
        let mut i = tail - 1;
        while i >= 0 {
            plan.route[len + i].x = plan.route[target + 1 + i].x;
            plan.route[len + i].y = plan.route[target + 1 + i].y;
            plan.route[len + i].link = plan.route[target + 1 + i].link;
            i -= 1;
        }
    } else {
        for i in 0..tail {
            plan.route[len + i].x = plan.route[target + 1 + i].x;
            plan.route[len + i].y = plan.route[target + 1 + i].y;
            plan.route[len + i].link = plan.route[target + 1 + i].link;
        }
    }
    t = q.tile_found;
    let mut i = len - 1;
    while t >= 0 {
        plan.route[i].x = q.tile_ox + t % q.tile_stride;
        plan.route[i].y = q.tile_oy + t / q.tile_stride;
        plan.route[i].link = 0;
        i -= 1;
        t = q.window.parent[t];
    }
    if plan.route.len() > total {
        plan.route.0.resize(total as usize, NavRouteTile::default());
    }
    plan.path_pos = 0;
    plan.cost = remaining;
    plan.path_cost = remaining;
    plan.walked_cost = 0;
    if total > 1 {
        return nav_step_to(g, si, plan.route[1].x, plan.route[1].y);
    }
    NavStep::Reached
}
