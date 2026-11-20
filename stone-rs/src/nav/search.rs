use super::*;

pub(super) fn nav_query_next_stamp(q: &mut NavQuery) {
    if q.stamp >= NAV_STAMP_LIMIT {
        for i in 0..q.node_stamp.len() {
            q.node_stamp[i] = 0;
            q.node_closed[i] = 0;
        }
        for i in 0..q.goal_stamp.len() {
            q.goal_stamp[i] = 0;
        }
        for i in 0..q.pad_h_stamp.len() {
            q.pad_h_stamp[i] = 0;
        }
        for i in 0..q.area_rank_stamp.len() {
            q.area_rank_stamp[i] = 0;
        }
        for i in 0..q.cert_stamp.len() {
            q.cert_stamp[i] = 0;
            q.cert_closed[i] = 0;
        }
        for i in 0..q.comp_mark.len() {
            q.comp_mark[i] = 0;
        }
        q.stamp = 0;
    }
    q.stamp += 1;
}

pub(super) fn nav_query_begin(g: &NavGraph, q: &mut NavQuery, plan: &NavPlan) {
    let label_limit = nav_graph_label_limit(g);
    while q.comp_mark.len() < label_limit {
        q.comp_mark.push(0);
    }
    if q.goal_field_epoch != g.epoch || q.goal_field_count >= 1024 {
        q.goal_fields.clear();
        q.goal_field_count = 0;
        q.goal_field_epoch = g.epoch;
    }
    q.area_prepared = false;
    q.area_pair = -1;
    q.area_building = false;
    q.area_choice = -1;
    q.area_target = -1;
    q.active = q.head;
    q.phase = NAV_Q_START;
    q.epoch = g.epoch;
    nav_query_next_stamp(q);
    q.start_count = 0;
    q.goal_count = 0;
    q.direct = NAV_INF;
    q.best = NAV_INF;
    q.best_node = -1;
    q.expansions = 0;
    q.heap.clear();
    q.pad_count = 0;
    q.pad_cost = NAV_INF;
    for l in 0..g.link_cost.len() {
        if (plan.teleports & g.link_bit[l]) == 0 {
            continue;
        }
        let pad = g.ep_base + 2 * l;
        let x = g.node_x[pad];
        let y = g.node_y[pad];
        let mut i = q.pad_count;
        nav_put(&mut q.pad_x, i, x);
        nav_put(&mut q.pad_y, i, y);
        while i > 0 && q.pad_x[i - 1] > x {
            q.pad_x[i] = q.pad_x[i - 1];
            q.pad_y[i] = q.pad_y[i - 1];
            i -= 1;
        }
        q.pad_x[i] = x;
        q.pad_y[i] = y;
        q.pad_count += 1;
        q.pad_cost = min(q.pad_cost, g.link_cost[l]);
    }
}

pub(super) fn nav_query_attach_start(g: &NavGraph, q: &mut NavQuery, plan: &NavPlan) -> int {
    if q.area_prepared {
        if let Some(work) = q.areas.work.as_mut() {
            work.repeated_attachments += 1;
        }
    }
    let si = plan.si;
    let cs = nav_tile_cluster(g, si, plan.from_x, plan.from_y);
    q.start_cluster = cs;
    let ox = g.space_ox[si];
    let oy = g.space_oy[si];
    let work = nav_tile_search(
        g,
        q,
        cs,
        cs,
        IRect {
            x: ox + plan.from_x,
            y: oy + plan.from_y,
            w: 1,
            h: 1,
        },
        true,
        IRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        false,
    );
    for i in 0..nav_cluster_slot_limit(g, cs) {
        let n = nav_cluster_node(g, cs, i);
        if n < 0 {
            continue;
        }
        let d = nav_tile_dist(q, g.node_x[n], g.node_y[n]);
        if d < 0 {
            continue;
        }
        nav_put(&mut q.start_node, q.start_count, n);
        nav_put(&mut q.start_cost, q.start_count, d);
        q.start_count += 1;
    }
    if plan.goal_si == si && plan.goal_w > 0 {
        let x0 = max(plan.goal_x, g.cluster_x[cs]);
        let y0 = max(plan.goal_y, g.cluster_y[cs]);
        let x1 = min(plan.goal_x + plan.goal_w, g.cluster_x[cs] + g.cluster_size);
        let y1 = min(plan.goal_y + plan.goal_h, g.cluster_y[cs] + g.cluster_size);
        if x1 - x0 == 1 && y1 - y0 == 1 {
            nav_direct_at(q, ox + x0, oy + y0);
        } else {
            for y in y0..y1 {
                for x in x0..x1 {
                    nav_direct_at(q, ox + x, oy + y);
                }
            }
        }
    }
    let s = g.cluster_size;
    q.gcx0 = plan.goal_x / s;
    q.gcy0 = plan.goal_y / s;
    q.gcx1 = (plan.goal_x + plan.goal_w - 1) / s;
    q.gcy1 = (plan.goal_y + plan.goal_h - 1) / s;
    q.gc_cursor = 0;
    q.phase = NAV_Q_GOAL;
    work
}

pub(super) fn nav_direct_at(q: &mut NavQuery, x: int, y: int) {
    let d = nav_tile_dist(q, x, y);
    if d >= 0 && d < q.direct {
        q.direct = d;
    }
}

pub(super) fn nav_query_goal_key(plan: &NavPlan) -> int {
    let axis = SPACE_SIZE + 1;
    ((plan.goal_x * axis + plan.goal_y) * axis + plan.goal_w) * axis + plan.goal_h
}

pub(super) fn nav_query_reuse_goal(q: &mut NavQuery, c: int, plan: &NavPlan) -> int {
    let key = nav_query_goal_key(plan);
    let Some(entry) = q
        .goal_fields
        .get(&c)
        .and_then(|fields| fields.get(&key))
        .cloned()
    else {
        return -1;
    };
    if entry.teleports != plan.teleports {
        return -1;
    }
    for i in 0..entry.nodes.len() {
        let n = entry.nodes[i];
        q.goal_cost[n] = entry.costs[i];
        q.goal_stamp[n] = q.stamp;
        nav_put(&mut q.goal_node, q.goal_count, n);
        q.goal_count += 1;
    }
    q.goal_hits += 1;
    // Cache reuse changes CPU work, never admission or answer ticks.
    entry.work
}

pub(super) fn nav_query_attach_goal(g: &NavGraph, q: &mut NavQuery, plan: &NavPlan) -> int {
    let wc = q.gcx1 - q.gcx0 + 1;
    let total = if plan.goal_w > 0 {
        wc * (q.gcy1 - q.gcy0 + 1)
    } else {
        0
    };
    if q.gc_cursor >= total {
        q.phase = NAV_Q_CHECK;
        return 0;
    }
    let gsi = plan.goal_si;
    let c = g.space_cluster_base[gsi]
        + (q.gcy0 + q.gc_cursor / wc) * g.span
        + q.gcx0
        + q.gc_cursor % wc;
    q.gc_cursor += 1;
    let reused = nav_query_reuse_goal(q, c, plan);
    if reused >= 0 {
        return reused;
    }
    let x0 = max(plan.goal_x, g.cluster_x[c]);
    let y0 = max(plan.goal_y, g.cluster_y[c]);
    let x1 = min(plan.goal_x + plan.goal_w, g.cluster_x[c] + g.cluster_size);
    let y1 = min(plan.goal_y + plan.goal_h, g.cluster_y[c] + g.cluster_size);
    q.goal_searches += 1;
    let work = nav_tile_search(
        g,
        q,
        c,
        c,
        IRect {
            x: g.space_ox[gsi] + x0,
            y: g.space_oy[gsi] + y0,
            w: x1 - x0,
            h: y1 - y0,
        },
        false,
        IRect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        false,
    );
    let mut entry = NavGoalAttachment {
        teleports: plan.teleports,
        work,
        ..NavGoalAttachment::default()
    };
    for i in 0..nav_cluster_slot_limit(g, c) {
        let n = nav_cluster_node(g, c, i);
        if n < 0 {
            continue;
        }
        let d = nav_tile_dist(q, g.node_x[n], g.node_y[n]);
        if d < 0 {
            continue;
        }
        q.goal_cost[n] = d;
        q.goal_stamp[n] = q.stamp;
        nav_put(&mut q.goal_node, q.goal_count, n);
        q.goal_count += 1;
        entry.nodes.push(n);
        entry.costs.push(d);
    }
    let key = nav_query_goal_key(plan);
    let is_new = q
        .goal_fields
        .get(&c)
        .and_then(|fields| fields.get(&key))
        .is_none();
    if is_new {
        q.goal_field_count += 1;
    }
    q.goal_fields.entry(c).or_default().insert(key, entry);
    work
}

pub(super) fn nav_query_check(g: &NavGraph, q: &mut NavQuery, plan: &mut NavPlan) -> int {
    let stamp = q.stamp;
    for i in 0..q.start_count {
        q.comp_mark[g.component[q.start_node[i]]] = stamp;
    }
    let mut joined = false;
    for i in 0..q.goal_count {
        if q.comp_mark[g.component[q.goal_node[i]]] == stamp {
            joined = true;
            break;
        }
    }
    q.best = q.direct;
    if !joined {
        nav_query_finish(g, q, plan);
        return 1;
    }
    if !q.area_prepared
        && q.gcx0 == q.gcx1
        && q.gcy0 == q.gcy1
        && plan.goal_si == g.cluster_space[q.start_cluster]
        && q.gcx0 == g.cluster_x[q.start_cluster] / g.cluster_size
        && q.gcy0 == g.cluster_y[q.start_cluster] / g.cluster_size
        && q.direct < NAV_INF
    {
        q.area_prepared = true;
    }
    let mut area_work = 0;
    if !q.area_prepared {
        let start_cluster = q.start_cluster;
        area_work += nav_area_ensure(g, q, start_cluster);
        for i in 0..q.goal_count {
            let cluster = nav_node_cluster(g, q.goal_node[i]);
            area_work += nav_area_ensure(g, q, cluster);
        }
        q.area_prepared = true;
        q.area_pair = nav_area_pair(g, q, plan);
        if q.area_pair >= 0 {
            let pair = &q.areas.pairs[q.area_pair];
            let from = pair.from;
            let to = pair.to;
            let ready = pair.ready;
            let count = pair.count;
            q.area_target = route_hash(plan.seed, from, to) % NAV_AREA_ROUTE_LIMIT;
            if !ready && count <= q.area_target {
                q.area_building = true;
                nav_area_search_begin(g, q, plan);
                return area_work + 1;
            }
            if count > 0 {
                q.area_choice = min(q.area_target, count - 1);
            }
        }
    }
    if q.area_choice >= 0 {
        let route = &q.areas.pairs[q.area_pair].routes[q.area_choice].areas;
        for i in 0..route.len() {
            q.area_rank[route[i]] = i;
            q.area_rank_stamp[route[i]] = q.stamp;
        }
    }
    q.phase = NAV_Q_SEARCH;
    for i in 0..q.start_count {
        let node = q.start_node[i];
        let cost = q.start_cost[i];
        nav_offer(g, q, plan, -1, node, cost, 0);
    }
    q.start_count + area_work + 1
}

pub(super) fn nav_area_search_begin(g: &NavGraph, q: &mut NavQuery, plan: &NavPlan) {
    if let Some(work) = q.areas.work.as_mut() {
        work.searches += 1;
    }
    nav_query_next_stamp(q);
    q.best = NAV_INF;
    q.best_node = -1;
    q.heap.clear();
    let from = q.areas.pairs[q.area_pair].from;
    let to = q.areas.pairs[q.area_pair].to;
    let goal = nav_area_anchor(q, to);
    q.area_goal = goal;
    q.goal_stamp[goal] = q.stamp;
    let start = nav_area_anchor(q, from);
    nav_offer(g, q, plan, -1, start, 0, 0);
    q.phase = NAV_Q_SEARCH;
}

pub(super) fn nav_area_resume(q: &mut NavQuery) {
    nav_query_next_stamp(q);
    for i in 0..q.goal_count {
        let n = q.goal_node[i];
        q.goal_stamp[n] = q.stamp;
    }
    q.best = q.direct;
    q.best_node = -1;
    q.heap.clear();
    q.phase = NAV_Q_CHECK;
}

fn nav_offer(
    g: &NavGraph,
    q: &mut NavQuery,
    plan: &NavPlan,
    from: int,
    v: int,
    gv: int,
    via_link: int,
) {
    let stamp = q.stamp;
    if from >= 0 && !nav_area_edge_allowed(q, from, v) {
        return;
    }
    if q.node_closed[v] == stamp || (q.node_stamp[v] == stamp && gv >= q.node_g[v]) {
        return;
    }
    q.node_g[v] = gv;
    q.node_stamp[v] = stamp;
    q.node_parent[v] = from;
    q.node_link[v] = via_link;
    let x = g.node_x[v];
    let y = g.node_y[v];
    let mut h = if q.area_building {
        nav_octile(x - g.node_x[q.area_goal], y - g.node_y[q.area_goal])
    } else {
        nav_goal_octile(plan, x, y)
    };
    if q.pad_count > 0 {
        if q.pad_h_stamp[v] != stamp {
            q.pad_h[v] = min(NAV_INF, nav_nearest_pad(q, x, y)) + q.pad_cost;
            q.pad_h_stamp[v] = stamp;
        }
        h = min(h, q.pad_h[v]);
    }
    q.heap.push(gv + h, gv, v);
}

fn nav_expand(g: &NavGraph, q: &mut NavQuery, plan: &NavPlan, n: int, gn: int) -> int {
    let k = g.k;
    let mut spent = 0;
    let c = nav_node_cluster(g, n);
    let i = nav_node_slot(g, n);
    let edge = |q: &mut NavQuery, v: int, cost: int, link: int| {
        let edge_cost = nav_area_edge_cost(g, q, plan, n, v, cost);
        nav_offer(g, q, plan, n, v, gn + edge_cost, link);
    };
    if i < k {
        let partner = nav_node_partner(g, n);
        if q.area_building || q.area_choice >= 0 {
            spent += nav_area_ensure(g, q, nav_node_cluster(g, partner));
        }
        edge(q, partner, nav_node_partner_cost(g, n), 0);
        for j in 0..k {
            let cost = nav_intra_cost(g, c, i, j);
            if cost < 0 || j == i {
                continue;
            }
            let v = nav_cluster_node(g, c, j);
            if v >= 0 {
                edge(q, v, cost, 0);
            }
        }
    } else if nav_node_is_pad(g, n) {
        let l = nav_node_link(g, n);
        if (plan.teleports & g.link_bit[l]) != 0 {
            if q.area_building || q.area_choice >= 0 {
                spent += nav_area_ensure(g, q, nav_node_cluster(g, n + 1));
            }
            edge(q, n + 1, g.link_cost[l], l + 1);
        }
    }
    let first = if i < k { k } else { 0 };
    for j in first..nav_cluster_slot_limit(g, c) {
        if j == i {
            continue;
        }
        let cost = nav_intra_cost(g, c, i, j);
        let v = nav_cluster_node(g, c, j);
        if cost >= 0 && !nav_node_blocked_endpoint(g, v) {
            edge(q, v, cost, 0);
        }
    }
    spent
}

pub(super) fn nav_query_search(
    g: &NavGraph,
    q: &mut NavQuery,
    plan: &mut NavPlan,
    allowance: int,
) -> int {
    let stamp = q.stamp;
    let mut spent = 0;
    while !q.heap.is_empty() {
        if spent >= allowance {
            return spent;
        }
        let HeapEntry { f, g: gn, n } = q.heap.pop().unwrap();
        if q.node_closed[n] == stamp || gn > q.node_g[n] {
            continue;
        }
        if f >= q.best {
            break;
        }
        spent += 1;
        q.node_closed[n] = stamp;
        q.expansions += 1;
        if q.goal_stamp[n] == stamp {
            let cost = if q.area_building { 0 } else { q.goal_cost[n] };
            if gn + cost < q.best {
                q.best = gn + cost;
                q.best_node = n;
            }
        }
        spent += nav_expand(g, q, plan, n, gn);
    }
    if q.area_building {
        let more = nav_area_accept(g, q);
        let pair_index = q.area_pair;
        let pair_count = q.areas.pairs[pair_index].count;
        if !more {
            q.area_building = false;
            q.areas.pairs[pair_index].ready = true;
            if pair_count > 0 {
                q.area_choice = min(q.area_target, pair_count - 1);
            }
            nav_area_resume(q);
        } else if pair_count > q.area_target {
            q.area_building = false;
            q.area_choice = q.area_target;
            nav_area_resume(q);
        } else {
            nav_area_search_begin(g, q, plan);
        }
    } else {
        nav_query_finish(g, q, plan);
    }
    spent + 1
}

pub(super) fn nav_octile(dx: int, dy: int) -> int {
    let ax = dx.abs();
    let ay = dy.abs();
    NAV_COST_STRAIGHT * max(ax, ay) + (NAV_COST_DIAGONAL - NAV_COST_STRAIGHT) * min(ax, ay)
}

pub(super) fn nav_goal_octile(plan: &NavPlan, x: int, y: int) -> int {
    let mut dx = 0;
    if x < plan.goal_wx {
        dx = plan.goal_wx - x;
    } else if x >= plan.goal_wx + plan.goal_w {
        dx = x - (plan.goal_wx + plan.goal_w - 1);
    }
    let mut dy = 0;
    if y < plan.goal_wy {
        dy = plan.goal_wy - y;
    } else if y >= plan.goal_wy + plan.goal_h {
        dy = y - (plan.goal_wy + plan.goal_h - 1);
    }
    nav_octile(dx, dy)
}

pub(super) fn nav_add_version(g: &NavGraph, plan: &mut NavPlan, c: int) {
    let n = plan.ver_count;
    if n > 0 && plan.ver_cluster[n - 1] == c {
        return;
    }
    nav_put(&mut plan.ver_cluster, n, c);
    nav_put(&mut plan.ver_value, n, g.cluster_version[c]);
    plan.ver_count = n + 1;
}

pub(super) fn nav_query_finish(g: &NavGraph, q: &mut NavQuery, plan: &mut NavPlan) {
    plan.expansions = q.expansions;
    plan.epoch = g.epoch;
    plan.wp_count = 0;
    plan.wp_next = 0;
    plan.ver_count = 0;
    plan.seg_count = 0;
    plan.seg_pos = 0;
    if q.best >= NAV_INF {
        nav_unlink(q, plan);
        plan.state = NAV_PLAN_UNREACHABLE;
        plan.cost = -1;
        return;
    }
    plan.cost = q.best;
    let mut len = 0;
    let mut n = q.best_node;
    while n >= 0 {
        len += 1;
        n = q.node_parent[n];
    }
    for v in [
        &mut plan.wp_x,
        &mut plan.wp_y,
        &mut plan.wp_cluster,
        &mut plan.wp_link,
    ] {
        while v.len() < len {
            v.push(0);
        }
        for i in 0..len {
            v[i] = 0;
        }
    }
    n = q.best_node;
    let mut i = len - 1;
    plan.last_link = -1;
    while n >= 0 {
        plan.wp_x[i] = g.node_x[n];
        plan.wp_y[i] = g.node_y[n];
        if n < g.corner_base {
            let partner = nav_node_partner(g, n);
            let offset = g.gap_lo[n]
                + route_hash(plan.seed, n.min(partner), 971) % (g.gap_hi[n] - g.gap_lo[n] + 1);
            let border = n / (2 * g.per_side);
            if g.border_vertical[border] {
                plan.wp_y[i] += offset;
            } else {
                plan.wp_x[i] += offset;
            }
        }
        plan.wp_cluster[i] = nav_node_cluster(g, n);
        plan.wp_link[i] = q.node_link[n];
        if q.node_link[n] != 0 && plan.last_link < 0 {
            plan.last_link = i;
        }
        i -= 1;
        n = q.node_parent[n];
    }
    plan.wp_count = len;
    plan.final_cluster = q.start_cluster;
    if q.best_node >= 0 {
        plan.final_cluster = nav_node_cluster(g, q.best_node);
    }
    nav_add_version(g, plan, q.start_cluster);
    for w in 0..len {
        let cluster = plan.wp_cluster[w];
        nav_add_version(g, plan, cluster);
    }
    let final_cluster = plan.final_cluster;
    nav_add_version(g, plan, final_cluster);
    plan.route.clear();
    plan.path_pos = 0;
    plan.path_cost = 0;
    plan.walked_cost = 0;
    let start_x = g.space_ox[plan.si] + plan.from_x;
    let start_y = g.space_oy[plan.si] + plan.from_y;
    nav_path_push(g, plan, start_x, start_y, 0);
    q.phase = NAV_Q_MATERIALIZE;
}

pub(super) fn nav_route_ready(g: &NavGraph, q: &mut NavQuery, plan: &mut NavPlan) {
    nav_route_finish_shape(g, plan);
    let capacity = plan.route.len() + 4 * g.cluster_size * g.cluster_size;
    nav_route_reserve(plan, capacity);
    plan.cost = plan.path_cost;
    plan.state = NAV_PLAN_READY;
    plan.path_pos = 0;
    plan.walked_cost = 0;
    nav_unlink(q, plan);
}

// Following the accepted route never searches or allocates scratch.
pub(super) fn nav_materialize(g: &NavGraph, q: &mut NavQuery, plan: &mut NavPlan) -> int {
    let p = plan.route.len() - 1;
    let wx = plan.route[p].x;
    let wy = plan.route[p].y;
    let si = nav_world_space(g, wx, wy, plan.si);
    let step = nav_refine(g, q, plan, si, wx - g.space_ox[si], wy - g.space_oy[si]);
    match step {
        NavStep::Step(_) => {
            for i in 1..plan.seg_count {
                let x = plan.seg_x[i];
                let y = plan.seg_y[i];
                nav_path_push(g, plan, x, y, 0);
            }
            max(1, q.refine_work)
        }
        NavStep::Teleport(_) => {
            let w = plan.wp_next;
            let link = plan.wp_link[w];
            let x = plan.wp_x[w];
            let y = plan.wp_y[w];
            nav_path_push(g, plan, x, y, link);
            plan.path_cost += g.link_cost[link - 1];
            plan.wp_next += 1;
            1
        }
        NavStep::Reached => {
            plan.wp_next = 0;
            let start_x = g.space_ox[plan.si] + plan.from_x;
            let start_y = g.space_oy[plan.si] + plan.from_y;
            if q.pad_count == 0
                && plan.path_cost * 10 <= nav_goal_octile(plan, start_x, start_y) * 13
            {
                nav_route_ready(g, q, plan);
            } else {
                nav_cert_begin(g, q, plan);
            }
            max(1, q.refine_work)
        }
        _ => panic!("nav abstract route could not be refined"),
    }
}

// Pads are sorted by x when the query begins.
pub(super) fn nav_nearest_pad(q: &NavQuery, x: int, y: int) -> int {
    if q.pad_count == 0 {
        return NAV_INF;
    }
    let mut lo = 0;
    let mut hi = q.pad_count;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if q.pad_x[mid] < x {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut left = lo - 1;
    let mut right = lo;
    let mut best = nav_octile(q.pad_x[0] - x, q.pad_y[0] - y);
    while left >= 0 || right < q.pad_count {
        let from_right =
            left < 0 || (right < q.pad_count && q.pad_x[right] - x < x - q.pad_x[left]);
        let p = if from_right { right } else { left };
        let dx = (q.pad_x[p] - x).abs();
        if dx > best / NAV_COST_STRAIGHT {
            return best;
        }
        best = min(best, nav_octile(dx, q.pad_y[p] - y));
        if from_right {
            right += 1;
        } else {
            left -= 1;
        }
    }
    best
}

pub(super) fn nav_cert_h(q: &NavQuery, plan: &NavPlan, x: int, y: int) -> int {
    let mut h = nav_goal_octile(plan, x, y);
    if q.pad_count > 0 {
        h = min(h, nav_nearest_pad(q, x, y) + q.pad_cost);
    }
    h
}

pub(super) fn nav_cert_begin(g: &NavGraph, q: &mut NavQuery, plan: &NavPlan) {
    q.heap.clear();
    q.phase = NAV_Q_CERTIFY;
    let t = g.space_tile_base[plan.si] + plan.from_y * SPACE_SIZE + plan.from_x;
    if nav_cert_offer(q, plan, t, 0, -1, 0) {
        let h = nav_cert_h(
            q,
            plan,
            g.space_ox[plan.si] + plan.from_x,
            g.space_oy[plan.si] + plan.from_y,
        );
        q.heap.push(h, 0, t);
    }
}

pub(super) fn nav_cert_offer(
    q: &mut NavQuery,
    plan: &NavPlan,
    t: int,
    cost: int,
    parent: int,
    link: int,
) -> bool {
    if q.cert_closed[t] == q.stamp {
        return false;
    }
    if q.cert_stamp[t] == q.stamp && q.cert_dist[t] <= cost {
        if q.cert_dist[t] == cost
            && route_hash(plan.seed, parent, t) > route_hash(plan.seed, q.cert_parent[t], t)
        {
            q.cert_parent[t] = parent;
            q.cert_link[t] = link;
        }
        return false;
    }
    q.cert_stamp[t] = q.stamp;
    q.cert_dist[t] = cost;
    q.cert_parent[t] = parent;
    q.cert_link[t] = link;
    true
}

pub(super) fn nav_certify(g: &NavGraph, q: &mut NavQuery, plan: &mut NavPlan, budget: int) -> int {
    let size = SPACE_SIZE;
    let mut spent = 0;
    let offer = |q: &mut NavQuery, t: int, nt: int, offered: int, via: int| {
        if nav_cert_offer(q, plan, nt, offered, t, via) {
            let ns = nt / (size * size);
            let x = g.space_ox[ns] + nt % size;
            let y = g.space_oy[ns] + (nt / size) % size;
            let h = nav_cert_h(q, plan, x, y);
            q.heap.push(offered + h, 0, nt);
        }
    };
    loop {
        if q.heap.is_empty() {
            panic!("nav route certificate exhausted a reachable graph");
        }
        if spent >= budget {
            return spent;
        }
        let HeapEntry { f, n: t, .. } = q.heap.pop().unwrap();
        if q.cert_closed[t] == q.stamp {
            continue;
        }
        let cost = q.cert_dist[t];
        if plan.path_cost * 10 <= f * 13 {
            nav_route_ready(g, q, plan);
            return spent + 1;
        }
        q.cert_closed[t] = q.stamp;
        spent += 1;
        let si = t / (size * size);
        let x = t % size;
        let y = (t / size) % size;
        if si == plan.goal_si
            && x >= plan.goal_x
            && y >= plan.goal_y
            && x < plan.goal_x + plan.goal_w
            && y < plan.goal_y + plan.goal_h
        {
            nav_cert_retain(g, q, plan, t, cost);
            nav_route_ready(g, q, plan);
            return spent;
        }
        let wx = g.space_ox[si] + x;
        let wy = g.space_oy[si] + y;
        for direction in [0, 1, 2, 3, 5, 6, 7, 8] {
            let dx = direction % 3 - 1;
            let dy = direction / 3 - 1;
            let nx = wx + dx;
            let ny = wy + dy;
            if nav_world_step_open(g, wx, wy, nx, ny, si) {
                let ns = nav_world_space(g, nx, ny, si);
                let nt = g.space_tile_base[ns] + (ny - g.space_oy[ns]) * size + nx - g.space_ox[ns];
                let hop = if dx != 0 && dy != 0 {
                    NAV_COST_DIAGONAL
                } else {
                    NAV_COST_STRAIGHT
                };
                offer(
                    q,
                    t,
                    nt,
                    cost + nav_edge_cost(hop, g.extra[t], g.extra[nt]),
                    0,
                );
            }
        }
        let mut l = g.tile_link_head[t];
        while l >= 0 {
            q.cert_link_visits += 1;
            if (plan.teleports & g.link_bit[l]) != 0 {
                offer(q, t, g.link_to_tile[l], cost + g.link_cost[l], l + 1);
            }
            l = g.link_next[l];
        }
    }
}

pub(super) fn nav_cert_retain(
    g: &NavGraph,
    q: &NavQuery,
    plan: &mut NavPlan,
    last: int,
    cost: int,
) {
    let mut len = 0;
    let mut t = last;
    while t >= 0 {
        len += 1;
        t = q.cert_parent[t];
    }
    nav_route_reserve(plan, len);
    while plan.route.len() < len {
        plan.route.push(NavRouteTile::default());
    }
    while plan.route.len() > len {
        let _ = plan.route.pop();
    }
    plan.path_cost = cost;
    plan.ver_count = 0;
    let size = SPACE_SIZE;
    let mut i = len - 1;
    t = last;
    while t >= 0 {
        let si = t / (size * size);
        let x = t % size;
        let y = (t / size) % size;
        plan.route[i].x = g.space_ox[si] + x;
        plan.route[i].y = g.space_oy[si] + y;
        plan.route[i].link = q.cert_link[t];
        nav_add_version(g, plan, nav_tile_cluster(g, si, x, y));
        i -= 1;
        t = q.cert_parent[t];
    }
}
