use super::*;

pub(super) fn nav_area_claim(nodes: &mut IdxVec<int>, area: &mut NavArea, n: int) {
    if n < 0 || nodes[n] >= 0 {
        return;
    }
    nodes[n] = area.id;
    area.nodes.push(n);
}

// A local-slot representative is a stable ID even when clusters are visited
// in different orders. Only search-touched clusters allocate area records.
pub(super) fn nav_area_ensure(g: &NavGraph, q: &mut NavQuery, c: int) -> int {
    let a = &mut q.areas;
    if a.epoch != g.epoch {
        a.epoch = g.epoch;
        a.count = 0;
        a.pair_count = 0;
    }
    if a.cluster_epoch[c] == g.epoch {
        return 0;
    }
    if let Some(work) = a.work.as_mut() {
        work.derivations += 1;
    }
    a.cluster_epoch[c] = g.epoch;
    let slots = nav_cluster_slot_limit(g, c);
    for j in 0..slots {
        let n = nav_cluster_node(g, c, j);
        if n >= 0 {
            a.nodes[n] = -1;
        }
    }
    let mut spent = 1;
    for j in 0..slots {
        let first = nav_cluster_node(g, c, j);
        if first < 0 || a.nodes[first] >= 0 {
            continue;
        }
        if a.count == a.areas.len() {
            a.areas.push(NavArea::default());
        }
        let count = a.count;
        let area = &mut a.areas[count];
        area.nodes.clear();
        area.id = first;
        a.index[first] = a.count;
        a.count += 1;
        nav_area_claim(&mut a.nodes, area, first);
        let mut flood = 0;
        while flood < area.nodes.len() {
            let n = area.nodes[flood];
            flood += 1;
            let slot = nav_node_slot(g, n);
            if !nav_node_blocked_endpoint(g, n) {
                for k in 0..slots {
                    let v = nav_cluster_node(g, c, k);
                    if v >= 0
                        && !nav_node_blocked_endpoint(g, v)
                        && nav_intra_cost(g, c, slot, k) >= 0
                    {
                        nav_area_claim(&mut a.nodes, area, v);
                    }
                }
            }
            spent += 1;
        }
        // Fixed medoids make cached routes independent of query endpoints.
        let mut best = 2_000_000_000;
        for i in 0..area.nodes.len() {
            let candidate = area.nodes[i];
            let candidate_slot = nav_node_slot(g, candidate);
            let mut sum = 0;
            for j in 0..area.nodes.len() {
                let other = area.nodes[j];
                if candidate != other {
                    sum += nav_intra_cost(g, c, candidate_slot, nav_node_slot(g, other));
                }
            }
            if sum < best {
                best = sum;
                area.anchor = candidate;
            }
        }
    }
    spent
}

pub(super) fn nav_area_anchor(q: &NavQuery, id: int) -> int {
    q.areas.areas[q.areas.index[id]].anchor
}

// A region spanning several areas keeps its existing multi-goal search.
pub(super) fn nav_area_pair(_g: &NavGraph, q: &mut NavQuery, plan: &NavPlan) -> int {
    if q.start_count == 0 || q.goal_count == 0 {
        return -1;
    }
    let from = q.areas.nodes[q.start_node[0]];
    let to = q.areas.nodes[q.goal_node[0]];
    if from == to {
        return -1;
    }
    for i in 1..q.start_count {
        if q.areas.nodes[q.start_node[i]] != from {
            return -1;
        }
    }
    for i in 1..q.goal_count {
        if q.areas.nodes[q.goal_node[i]] != to {
            return -1;
        }
    }
    let a = &mut q.areas;
    for i in 0..a.pair_count {
        let pair = &a.pairs[i];
        if pair.from == from && pair.to == to && pair.teleports == plan.teleports {
            if let Some(work) = a.work.as_mut() {
                work.hits += 1;
            }
            return i;
        }
    }
    // Bounded FIFO storage; an eviction regenerates the same seed-independent set.
    if let Some(work) = a.work.as_mut() {
        work.misses += 1;
    }
    let mut index = a.pair_count;
    if index >= NAV_AREA_CACHE_LIMIT {
        if let Some(work) = a.work.as_mut() {
            work.evictions += 1;
        }
        index = a.builds % NAV_AREA_CACHE_LIMIT;
    } else {
        a.pair_count += 1;
    }
    if index == a.pairs.len() {
        a.pairs.push(NavAreaPair::default());
    }
    let pair = &mut a.pairs[index];
    pair.from = from;
    pair.to = to;
    pair.teleports = plan.teleports;
    pair.count = 0;
    pair.attempts = 0;
    pair.ready = false;
    pair.shortest = -1;
    pair.penalty.clear();
    a.builds += 1;
    index
}

pub(super) fn nav_area_edge_cost(
    g: &NavGraph,
    q: &NavQuery,
    plan: &NavPlan,
    from: int,
    to: int,
    cost: int,
) -> int {
    if q.area_building {
        let a = q.areas.nodes[from];
        let b = q.areas.nodes[to];
        if a == b {
            return cost;
        }
        let key = a * nav_graph_node_limit(g) + b;
        let times = q.areas.pairs[q.area_pair]
            .penalty
            .get(&key)
            .copied()
            .unwrap_or(0);
        return cost + times * g.cluster_size * 1000;
    }
    let lo = from.min(to);
    let hi = from.max(to);
    cost + cost * (route_hash(plan.seed, lo, hi) % 201) / 1000
}

pub(super) fn nav_area_edge_allowed(q: &NavQuery, from: int, to: int) -> bool {
    if q.area_building || q.area_choice < 0 {
        return true;
    }
    let a = q.areas.nodes[from];
    let b = q.areas.nodes[to];
    // Seeds and every admitted crossing already belong to the chosen sequence.
    if a == b {
        return true;
    }
    q.area_rank_stamp[b] == q.stamp && q.area_rank[b] == q.area_rank[a] + 1
}

// Extract an unpenalized abstract cost and deduplicated area sequence.
// Reject loops and stop beyond the 1.3x bound of the area's cheapest route.
pub(super) fn nav_area_accept(g: &NavGraph, q: &mut NavQuery) -> bool {
    let pair_index = q.area_pair;
    q.areas.pairs[pair_index].attempts += 1;
    if q.best_node < 0 {
        return false;
    }
    q.areas.candidate.clear();
    let mut cost = 0;
    let mut n = q.best_node;
    while n >= 0 {
        let area = q.areas.nodes[n];
        let len = q.areas.candidate.len();
        if len == 0 || q.areas.candidate[len - 1] != area {
            q.areas.candidate.push(area);
        }
        let parent = q.node_parent[n];
        if parent >= 0 {
            if q.node_link[n] != 0 {
                cost += g.link_cost[q.node_link[n] - 1];
            } else if nav_node_cluster(g, parent) == nav_node_cluster(g, n) {
                cost += nav_intra_cost(
                    g,
                    nav_node_cluster(g, n),
                    nav_node_slot(g, parent),
                    nav_node_slot(g, n),
                );
            } else {
                cost += nav_node_partner_cost(g, parent);
            }
        }
        n = parent;
    }
    if q.areas.pairs[pair_index].shortest < 0 {
        q.areas.pairs[pair_index].shortest = cost;
    }
    let shortest = q.areas.pairs[pair_index].shortest;
    if cost * 10 > shortest * 13 {
        return false;
    }
    let mut duplicate = false;
    for i in 0..q.areas.candidate.len() {
        for j in 0..i {
            if q.areas.candidate[i] == q.areas.candidate[j] {
                duplicate = true;
            }
        }
    }
    for r in 0..q.areas.pairs[pair_index].count {
        let route = &q.areas.pairs[pair_index].routes[r];
        if route.areas.len() != q.areas.candidate.len() {
            continue;
        }
        let mut equal = true;
        for i in 0..route.areas.len() {
            if route.areas[i] != q.areas.candidate[q.areas.candidate.len() - i - 1] {
                equal = false;
                break;
            }
        }
        if equal {
            duplicate = true;
        }
    }
    let node_limit = nav_graph_node_limit(g);
    for i in 1..q.areas.candidate.len() {
        let edge = q.areas.candidate[i] * node_limit + q.areas.candidate[i - 1];
        let pair = &mut q.areas.pairs[pair_index];
        let count = pair.penalty.get(&edge).copied().unwrap_or(0);
        pair.penalty.insert(edge, count + 1);
    }
    if !duplicate {
        let pair = &mut q.areas.pairs[pair_index];
        if pair.count == pair.routes.len() {
            pair.routes.push(NavAreaRoute::default());
        }
        let route = &mut pair.routes[pair.count];
        route.areas.clear();
        let candidate_len = q.areas.candidate.len();
        for i in 0..candidate_len {
            route.areas.push(q.areas.candidate[candidate_len - i - 1]);
        }
        route.cost = cost;
        pair.count += 1;
    }
    let pair = &q.areas.pairs[pair_index];
    pair.count < NAV_AREA_ROUTE_LIMIT && pair.attempts < 2 * NAV_AREA_ROUTE_LIMIT
}
