use super::*;

pub(super) fn nav_fill(v: &mut IdxVec<int>, n: int, value: int) {
    while v.len() < n {
        v.push(value);
    }
}

pub(super) fn nav_query_size(g: &NavGraph, q: &mut NavQuery) {
    let nodes = nav_graph_node_limit(g);
    macro_rules! size {
        ($f:ident, $n:expr, $v:expr) => {
            if q.$f.len() < $n {
                q.$f.0.resize($n as usize, $v);
            }
        };
    }
    if q.areas.nodes.len() < nodes {
        q.areas.nodes.0.resize(nodes as usize, -1);
    }
    if q.areas.index.len() < nodes {
        q.areas.index.0.resize(nodes as usize, -1);
    }
    nav_fill(&mut q.areas.cluster_epoch, g.cluster_space.len(), -1);
    size!(area_rank, nodes, 0);
    size!(area_rank_stamp, nodes, 0);
    size!(node_g, nodes, 0);
    size!(node_stamp, nodes, 0);
    size!(node_parent, nodes, -1);
    size!(node_closed, nodes, 0);
    size!(node_link, nodes, 0);
    size!(goal_cost, nodes, 0);
    size!(goal_stamp, nodes, 0);
    size!(pad_h, nodes, 0);
    size!(pad_h_stamp, nodes, 0);
    let window = 4 * g.cluster_size * g.cluster_size;
    size!(tile_dist, window, 0);
    size!(tile_stamp, window, 0);
    size!(tile_parent, window, -1);
    size!(open_cost, window, 0);
    size!(open_tile, window, 0);
    size!(open_slot, window, -1);
    let tiles = g.blocked.len();
    size!(cert_dist, tiles, 0);
    size!(cert_stamp, tiles, 0);
    size!(cert_parent, tiles, -1);
    size!(cert_link, tiles, 0);
    size!(cert_closed, tiles, 0);
}

fn nav_check_tile(g: &NavGraph, si: int, grid: IVec2) {
    assert!(
        nav_tile_cluster(g, si, grid.x, grid.y) >= 0,
        "nav tile {},{} is outside space {}",
        grid.x,
        grid.y,
        g.space_id[si]
    );
}

pub(super) fn nav_request(
    g: &NavGraph,
    q: &mut NavQuery,
    from: Place,
    goal: NavRegion,
    teleports: int,
    seed: int,
) -> NavPlanId {
    let si = nav_space_index(g, from.space);
    nav_check_tile(g, si, from.grid);
    let slot = if let Some(slot) = q.free.pop() {
        slot
    } else {
        let slot = q.plans.len();
        let mut plan = NavPlan {
            slot,
            ..NavPlan::default()
        };
        plan.route.reserve(8 * g.cluster_size * g.cluster_size);
        q.plans.push(plan);
        slot
    };
    let mut plan = std::mem::take(&mut q.plans[slot]);
    let gsi = nav_space_index(g, goal.space);
    let r = goal.rect;
    let x0 = max(r.x, 0);
    let y0 = max(r.y, 0);
    plan.goal_si = gsi;
    plan.goal_x = x0;
    plan.goal_y = y0;
    plan.goal_w = max(min(r.x + r.w, SPACE_SIZE) - x0, 0);
    plan.goal_h = max(min(r.y + r.h, SPACE_SIZE) - y0, 0);
    if plan.goal_w == 0 || plan.goal_h == 0 {
        plan.goal_w = 0;
        plan.goal_h = 0;
    }
    plan.goal_wx = g.space_ox[plan.goal_si] + plan.goal_x;
    plan.goal_wy = g.space_oy[plan.goal_si] + plan.goal_y;
    plan.seed = seed;
    plan.teleports = teleports;
    plan.expansions = 0;
    plan.cost = 0;
    plan.wp_count = 0;
    plan.ver_count = 0;
    let id = nav_requery_id(q, &mut plan, si, from.grid.x, from.grid.y);
    q.plans[slot] = plan;
    id
}

pub(super) fn nav_requery_id(
    q: &mut NavQuery,
    plan: &mut NavPlan,
    si: int,
    x: int,
    y: int,
) -> NavPlanId {
    if let Some(work) = &mut q.areas.work {
        work.requests += 1;
    }
    plan.si = si;
    plan.from_x = x;
    plan.from_y = y;
    plan.state = NAV_PLAN_QUEUED;
    plan.seg_count = 0;
    plan.seg_pos = 0;
    plan.queue_prev = q.tail;
    plan.queue_next = -1;
    if q.tail >= 0 {
        q.plans[q.tail].queue_next = plan.slot;
    } else {
        q.head = plan.slot;
    }
    q.tail = plan.slot;
    NavPlanId {
        slot: plan.slot,
        generation: plan.generation,
    }
}

pub(super) fn nav_requery(
    _g: &NavGraph,
    q: &mut NavQuery,
    plan: &mut NavPlan,
    si: int,
    x: int,
    y: int,
) -> NavStep {
    nav_requery_id(q, plan, si, x, y);
    NavStep::Pending
}

pub(super) fn nav_unlink(q: &mut NavQuery, plan: &mut NavPlan) {
    if plan.queue_prev >= 0 {
        q.plans[plan.queue_prev].queue_next = plan.queue_next;
    } else {
        q.head = plan.queue_next;
    }
    if plan.queue_next >= 0 {
        q.plans[plan.queue_next].queue_prev = plan.queue_prev;
    } else {
        q.tail = plan.queue_prev;
    }
    plan.queue_prev = -1;
    plan.queue_next = -1;
    if q.active == plan.slot {
        q.active = -1;
        q.phase = NAV_Q_IDLE;
    }
}

pub(super) fn nav_plan_get(q: &NavQuery, id: NavPlanId) -> &NavPlan {
    assert!(
        id.slot >= 0 && id.slot < q.plans.len(),
        "nav plan slot {} does not exist",
        id.slot
    );
    let plan = &q.plans[id.slot];
    assert!(
        plan.generation == id.generation && plan.state != NAV_PLAN_FREE,
        "nav plan {}:{} was released",
        id.slot,
        id.generation
    );
    plan
}

pub(super) fn nav_plan_release(q: &mut NavQuery, id: NavPlanId) {
    nav_plan_get(q, id);
    let mut plan = std::mem::take(&mut q.plans[id.slot]);
    if plan.state == NAV_PLAN_QUEUED {
        nav_unlink(q, &mut plan);
    }
    plan.state = NAV_PLAN_FREE;
    plan.generation += 1;
    q.free.push(plan.slot);
    q.plans[id.slot] = plan;
}

pub(super) fn nav_plan_pending(q: &NavQuery, id: NavPlanId) -> bool {
    nav_plan_get(q, id).state == NAV_PLAN_QUEUED
}

pub(super) fn nav_graph_advance(g: &NavGraph, q: &mut NavQuery, budget: int) -> bool {
    q.advance_spent = 0;
    if q.head < 0 {
        return true;
    }
    if nav_graph_refresh_pending(g) {
        return false;
    }
    let mut spent = 0;
    while q.head >= 0 {
        q.advance_spent = spent;
        if spent >= budget {
            return false;
        }
        let slot = q.head;
        let mut plan = std::mem::take(&mut q.plans[slot]);
        if q.active != q.head || q.epoch != g.epoch {
            nav_query_begin(g, q, &plan);
        }
        spent += match q.phase {
            NAV_Q_START => nav_query_attach_start(g, q, &mut plan),
            NAV_Q_GOAL => nav_query_attach_goal(g, q, &mut plan),
            NAV_Q_CHECK => nav_query_check(g, q, &mut plan),
            NAV_Q_SEARCH => nav_query_search(g, q, &mut plan, budget - spent),
            NAV_Q_MATERIALIZE => nav_materialize(g, q, &mut plan),
            _ => nav_certify(g, q, &mut plan, budget - spent),
        };
        q.plans[slot] = plan;
    }
    q.advance_spent = spent;
    true
}

pub(super) fn nav_plan_teleport_link(q: &NavQuery, id: NavPlanId) -> int {
    let p = nav_plan_get(q, id);
    let next = p.path_pos + 1;
    if p.state != NAV_PLAN_READY || next >= p.route.len() {
        -1
    } else {
        p.route[next].link - 1
    }
}
pub(super) fn nav_plan_cost(q: &NavQuery, id: NavPlanId) -> int {
    let p = nav_plan_get(q, id);
    if p.state != NAV_PLAN_READY {
        -1
    } else {
        p.cost
    }
}

pub(super) fn nav_route_hop_cost(g: &NavGraph, plan: &NavPlan, position: int) -> int {
    let to = plan.route[position];
    if to.link != 0 {
        return g.link_cost[to.link - 1];
    }
    let from = plan.route[position - 1];
    nav_hop_cost(g, plan, from.x, from.y, to.x, to.y)
}

pub(super) fn nav_plan_restore_route_costs(
    g: &NavGraph,
    plan: &mut NavPlan,
    saved_cost: Option<int>,
    saved_walked: int,
) {
    let (cost, walked) = if let Some(saved) = saved_cost {
        (saved, saved_walked)
    } else {
        let mut cost = 0;
        let mut walked = 0;
        for i in 1..plan.route.len() {
            let hop = nav_route_hop_cost(g, plan, i);
            cost += hop;
            if i <= plan.path_pos {
                walked += hop;
            }
        }
        (cost, walked)
    };
    plan.cost = cost;
    plan.path_cost = cost;
    plan.walked_cost = walked;
}

pub(super) fn nav_plan_remaining_cost(g: &NavGraph, plan: &NavPlan, at: Place) -> int {
    if plan.state != NAV_PLAN_READY {
        return -1;
    }
    let si = nav_space_index(g, at.space);
    let wx = g.space_ox[si] + at.grid.x;
    let wy = g.space_oy[si] + at.grid.y;
    let pos = plan.path_pos;
    if pos < plan.route.len() && plan.route[pos].x == wx && plan.route[pos].y == wy {
        return max(0, plan.cost - plan.walked_cost);
    }
    if pos + 1 < plan.route.len() && plan.route[pos + 1].x == wx && plan.route[pos + 1].y == wy {
        return max(
            0,
            plan.cost - plan.walked_cost - nav_route_hop_cost(g, plan, pos + 1),
        );
    }
    -1
}

pub(super) fn nav_plan_link(q: &NavQuery, id: NavPlanId, n: int) -> int {
    let plan = nav_plan_get(q, id);
    if plan.state != NAV_PLAN_READY {
        return -1;
    }
    let mut seen = 0;
    for w in 0..plan.route.len() {
        if plan.route[w].link != 0 {
            if seen == n {
                return plan.route[w].link - 1;
            }
            seen += 1;
        }
    }
    -1
}

pub(super) fn nav_next_step(
    g: &NavGraph,
    q: &mut NavQuery,
    plan: &mut NavPlan,
    at: Place,
) -> NavStep {
    let si = nav_space_index(g, at.space);
    let x = at.grid.x;
    let y = at.grid.y;
    if si == plan.goal_si
        && x >= plan.goal_x
        && y >= plan.goal_y
        && x < plan.goal_x + plan.goal_w
        && y < plan.goal_y + plan.goal_h
    {
        return NavStep::Reached;
    }
    if plan.state == NAV_PLAN_QUEUED {
        return NavStep::Pending;
    }
    if plan.state == NAV_PLAN_UNREACHABLE {
        if plan.epoch == g.epoch {
            return NavStep::Unreachable;
        }
        return nav_requery(g, q, plan, si, x, y);
    }
    if plan.epoch != g.epoch {
        for i in 0..plan.ver_count {
            if g.cluster_version[plan.ver_cluster[i]] != plan.ver_value[i] {
                return nav_requery(g, q, plan, si, x, y);
            }
        }
        plan.epoch = g.epoch;
    }
    let wx = g.space_ox[si] + x;
    let wy = g.space_oy[si] + y;
    let mut pos = plan.path_pos;
    if pos + 1 < plan.route.len() && plan.route[pos + 1].x == wx && plan.route[pos + 1].y == wy {
        plan.walked_cost += nav_route_hop_cost(g, plan, pos + 1);
        pos += 1;
        plan.path_pos = pos;
    } else if pos >= plan.route.len() || plan.route[pos].x != wx || plan.route[pos].y != wy {
        return nav_reconnect(g, q, plan, si, x, y);
    }
    if pos + 1 >= plan.route.len() {
        return NavStep::Reached;
    }
    let nx = plan.route[pos + 1].x;
    let ny = plan.route[pos + 1].y;
    if plan.route[pos + 1].link != 0 {
        return NavStep::Teleport(at);
    }
    if !nav_world_step_open(g, wx, wy, nx, ny, si) {
        let ca = nav_tile_cluster(g, si, x, y);
        let ns = nav_world_space(g, nx, ny, si);
        if nav_cluster_pending(g, ca)
            || (ns >= 0
                && nav_cluster_pending(
                    g,
                    nav_tile_cluster(g, ns, nx - g.space_ox[ns], ny - g.space_oy[ns]),
                ))
        {
            return NavStep::Blocked;
        }
        return nav_requery(g, q, plan, si, x, y);
    }
    nav_step_to(g, si, nx, ny)
}

pub(super) fn nav_query_follow_size(g: &NavGraph, q: &mut NavQuery) {
    let window = (4 * g.cluster_size * g.cluster_size) as usize;
    q.tile_dist.0.resize(window, 0);
    q.tile_stamp.0.resize(window, 0);
    q.tile_parent.0.resize(window, -1);
    q.open_cost.0.resize(window, 0);
    q.open_tile.0.resize(window, 0);
    q.open_slot.0.resize(window, -1);
}
