use super::*;

pub(super) fn nav_edge_cost(step: int, ea: int, eb: int) -> int {
    step + (ea + eb) * step / 1000
}

pub(super) fn nav_graph_owned_input_error(
    spaces: &[NavSpaceLayout],
    blocked: &IdxVec<bool>,
    corners: &IdxVec<int>,
    extra: &IdxVec<int>,
    cluster_size: int,
) -> Option<String> {
    let tiles = spaces.len() as int * SPACE_SIZE * SPACE_SIZE;
    if blocked.len() != tiles || corners.len() != tiles || extra.len() != tiles {
        return Some(format!(
            "nav owned tiles need {} blocked, corner and extra values, got {}, {} and {}",
            tiles,
            blocked.len(),
            corners.len(),
            extra.len()
        ));
    }
    nav_graph_layout_input_error(spaces, cluster_size)
}

pub(super) fn nav_graph_cluster_input_error(cluster_size: int) -> Option<String> {
    if cluster_size < 2 || SPACE_SIZE % cluster_size != 0 {
        return Some(format!(
            "nav cluster size must be at least 2 and divide the space size {}, got {}",
            SPACE_SIZE, cluster_size
        ));
    }
    None
}

pub(super) fn nav_graph_layout_input_error(
    spaces: &[NavSpaceLayout],
    cluster_size: int,
) -> Option<String> {
    if let Some(problem) = nav_graph_cluster_input_error(cluster_size) {
        return Some(problem);
    }
    let mut cells = BTreeMap::new();
    let mut ids = BTreeMap::new();
    for input in spaces {
        if let Some(problem) =
            nav_graph_space_input_error(input.space, input.origin, &mut cells, &mut ids)
        {
            return Some(problem);
        }
    }
    None
}

pub(super) fn nav_graph_space_input_error(
    space: SpaceId,
    origin: IVec2,
    cells: &mut BTreeMap<int, int>,
    ids: &mut BTreeMap<int, int>,
) -> Option<String> {
    let cx = origin.x.div_euclid(SPACE_SIZE);
    let cy = origin.y.div_euclid(SPACE_SIZE);
    if cx * SPACE_SIZE != origin.x || cy * SPACE_SIZE != origin.y {
        return Some(format!(
            "nav space {}: origin {},{} is not a cell corner",
            space.value, origin.x, origin.y
        ));
    }
    let key = nav_grid_key(cx, cy);
    if let Some(other) = cells.get(&key).copied() {
        return Some(format!(
            "nav spaces {} and {} share cell {},{}",
            other, space.value, cx, cy
        ));
    }
    cells.insert(key, space.value);
    if ids.contains_key(&space.value) {
        return Some(format!("nav space {} is given twice", space.value));
    }
    ids.insert(space.value, 1);
    None
}

pub(super) fn nav_graph_create_owned(
    spaces: &[NavSpaceLayout],
    blocked: IdxVec<bool>,
    corners: IdxVec<int>,
    extra: IdxVec<int>,
    links: &[NavLinkInput],
    cluster_size: int,
) -> NavGraph {
    if let Some(problem) =
        nav_graph_owned_input_error(spaces, &blocked, &corners, &extra, cluster_size)
    {
        panic!("{problem}");
    }
    nav_graph_layout_create(spaces, blocked, corners, extra, links, cluster_size)
}

pub(super) fn nav_graph_layout_create(
    spaces: &[NavSpaceLayout],
    blocked: IdxVec<bool>,
    corners: IdxVec<int>,
    extra: IdxVec<int>,
    links: &[NavLinkInput],
    cluster_size: int,
) -> NavGraph {
    let s = cluster_size;
    let span = SPACE_SIZE / s;
    let per_side = s - 1;
    let mut g = NavGraph {
        cluster_size: s,
        span,
        per_side,
        k: 4 * per_side + 4,
        blocked,
        corners,
        extra,
        ..NavGraph::default()
    };
    let clusters = spaces.len() as int * span * span;
    for si in 0..spaces.len() as int {
        let input = &spaces[si as usize];
        let ox = input.origin.x;
        let oy = input.origin.y;
        g.cell_space.insert(
            nav_grid_key(ox.div_euclid(SPACE_SIZE), oy.div_euclid(SPACE_SIZE)),
            si,
        );
        g.id_space.insert(input.space.value, si);
        g.space_id.push(input.space.value);
        g.space_ox.push(ox);
        g.space_oy.push(oy);
        g.space_tile_base.push(si * SPACE_SIZE * SPACE_SIZE);
        g.space_cluster_base.push(si * span * span);
        for cy in 0..span {
            for cx in 0..span {
                g.cluster_space.push(si);
                g.cluster_x.push(cx * s);
                g.cluster_y.push(cy * s);
                g.cluster_wx.push(ox + cx * s);
                g.cluster_wy.push(oy + cy * s);
            }
        }
    }
    g.cluster_border = IdxVec::filled(clusters * 4, -1);
    for c in 0..clusters {
        let east = nav_world_cluster(&g, g.cluster_wx[c] + s, g.cluster_wy[c]);
        if east >= 0 {
            nav_border_init(&mut g, true, c, east);
        }
        let south = nav_world_cluster(&g, g.cluster_wx[c], g.cluster_wy[c] + s);
        if south >= 0 {
            nav_border_init(&mut g, false, c, south);
        }
    }
    let borders = g.border_a.len();
    let k = g.k;
    let cost_slots = clusters * k * (k + 1) / 2;
    g.cost = IdxVec::filled(cost_slots, -1);
    g.border_count = IdxVec::filled(borders, 0);
    g.corner_base = borders * 2 * per_side;
    nav_vertices_init(&mut g);
    let vertices = g.vertex_x.len();
    g.ep_base = g.corner_base + 4 * vertices;
    g.pending = IdxVec::filled(clusters, false);
    g.cluster_version = IdxVec::filled(clusters, 0);
    g.cluster_ep_start = IdxVec::filled(clusters, 0);
    g.cluster_ep_count = IdxVec::filled(clusters, 0);
    g.round_full = IdxVec::filled(clusters, false);
    g.round_dir = IdxVec::filled(clusters * 4, false);
    g.round_corner = IdxVec::filled(clusters * 4, false);
    g.round_stamp = IdxVec::filled(clusters, 0);
    g.border_old = IdxVec::filled(per_side, [0; 4]);
    g.round_borders = IdxVec::filled(borders, 0);
    g.border_stamp = IdxVec::filled(borders, 0);
    g.corner_live = IdxVec::filled(vertices * 4, false);
    g.round_vertices = IdxVec::filled(vertices, 0);
    g.vertex_stamp = IdxVec::filled(vertices, 0);
    g.search_edges = IdxVec::filled(s * s * 9, None);
    g.search_ready = IdxVec::filled(s * s, false);
    g.window.reserve(s * s);
    nav_graph_set_links(&mut g, links);
    for v in 0..vertices {
        for q in 0..4 {
            let n = g.corner_base + v * 4 + q;
            g.node_x[n] = g.vertex_x[v] - if q == 1 || q == 2 { 1 } else { 0 };
            g.node_y[n] = g.vertex_y[v] - if q >= 2 { 1 } else { 0 };
        }
    }
    for c in 0..clusters {
        nav_mark_pending(&mut g, c);
    }
    g
}

// Coordinates must stay within 2^22.
pub(super) fn nav_grid_key(x: int, y: int) -> int {
    (x + 4194304) * 8388608 + (y + 4194304)
}

pub(super) fn nav_world_cluster(g: &NavGraph, wx: int, wy: int) -> int {
    let si = nav_world_space(g, wx, wy, -1);
    if si < 0 {
        return -1;
    }
    let s = g.cluster_size;
    g.space_cluster_base[si]
        + (wy - g.space_oy[si]).div_euclid(s) * g.span
        + (wx - g.space_ox[si]).div_euclid(s)
}

pub(super) fn nav_vertices_init(g: &mut NavGraph) {
    let clusters = g.cluster_space.len();
    let s = g.cluster_size;
    g.cluster_vertex = IdxVec::filled(clusters * 4, -1);
    for c in 0..clusters {
        let x = g.cluster_wx[c];
        let y = g.cluster_wy[c];
        let west = nav_world_cluster(g, x - s, y);
        let north_west = nav_world_cluster(g, x - s, y - s);
        let north = nav_world_cluster(g, x, y - s);
        if west < 0 || north_west < 0 || north < 0 {
            continue;
        }
        let id = g.vertex_x.len();
        g.vertex_x.push(x);
        g.vertex_y.push(y);
        g.vertex_cluster.push(c);
        g.vertex_cluster.push(west);
        g.vertex_cluster.push(north_west);
        g.vertex_cluster.push(north);
        g.cluster_vertex[c * 4] = id;
        g.cluster_vertex[west * 4 + 1] = id;
        g.cluster_vertex[north_west * 4 + 2] = id;
        g.cluster_vertex[north * 4 + 3] = id;
    }
}

pub(super) fn nav_border_init(g: &mut NavGraph, vertical: bool, a: int, bc: int) {
    let b = g.border_a.len();
    g.border_vertical.push(vertical);
    g.border_a.push(a);
    g.border_b.push(bc);
    let da = if vertical { 1 } else { 2 };
    let db = if vertical { 3 } else { 0 };
    if g.cluster_border[a * 4 + da] >= 0 || g.cluster_border[bc * 4 + db] >= 0 {
        panic!(
            "nav clusters {} and {} already have a border on that side",
            a, bc
        );
    }
    g.cluster_border[a * 4 + da] = b;
    g.cluster_border[bc * 4 + db] = b;
}

pub(super) fn nav_graph_set_links(g: &mut NavGraph, links: &[NavLinkInput]) {
    for e in 0..g.ep_cluster.len() {
        let c = g.ep_cluster[e];
        nav_mark_pending(g, c);
    }
    let clusters = g.cluster_ep_count.len();
    if g.tile_link_head.len() == 0 {
        g.tile_link_head.resize(g.blocked.len(), -1);
    }
    for i in 0..g.link_from_tile.len() {
        let t = g.link_from_tile[i];
        g.tile_link_head[t] = -1;
    }
    g.link_next.clear();
    g.link_from_tile.clear();
    g.link_to_tile.clear();
    g.link_cost.clear();
    g.link_bit.clear();
    g.ep_cluster.clear();
    let nb = g.ep_base;
    g.node_x.resize(nb, 0);
    g.node_y.resize(nb, 0);
    g.gap_lo.resize(nb, 0);
    g.gap_hi.resize(nb, 0);
    for link in links {
        if link.cost < 0 {
            panic!("nav link cost must not be negative, got {}", link.cost);
        }
        g.link_cost.push(link.cost);
        g.link_bit.push(link.requirement);
        nav_link_endpoint(g, link.from);
        nav_link_endpoint(g, link.to);
        let from_si = nav_space_index(g, link.from.space);
        let to_si = nav_space_index(g, link.to.space);
        g.link_from_tile.push(nav_tile_index(
            g,
            from_si,
            link.from.grid.x,
            link.from.grid.y,
        ));
        g.link_to_tile
            .push(nav_tile_index(g, to_si, link.to.grid.x, link.to.grid.y));
        g.link_next.push(-1);
    }
    let mut l = g.link_cost.len() - 1;
    while l >= 0 {
        let t = g.link_from_tile[l];
        g.link_next[l] = g.tile_link_head[t];
        g.tile_link_head[t] = l;
        l -= 1;
    }
    let eps = g.ep_cluster.len();
    for c in 0..clusters {
        g.cluster_ep_count[c] = 0;
    }
    for e in 0..eps {
        let c = g.ep_cluster[e];
        g.cluster_ep_count[c] += 1;
    }
    let mut at = 0;
    for c in 0..clusters {
        g.cluster_ep_start[c] = at;
        at += g.cluster_ep_count[c];
    }
    g.cluster_eps.resize(eps, 0);
    g.ep_index.resize(eps, 0);
    g.ep_row.resize(eps, 0);
    for c in 0..clusters {
        g.cluster_ep_count[c] = 0;
    }
    for e in 0..eps {
        let c = g.ep_cluster[e];
        let i = g.cluster_ep_count[c];
        g.cluster_eps[g.cluster_ep_start[c] + i] = e;
        g.ep_index[e] = i;
        g.cluster_ep_count[c] = i + 1;
    }
    let mut row = 0;
    for e in 0..eps {
        g.ep_row[e] = row;
        row += g.k + g.cluster_ep_count[g.ep_cluster[e]];
    }
    g.ep_cost.clear();
    g.ep_cost.resize(row, -1);
    g.ep_blocked.clear();
    for _ in 0..eps {
        g.ep_blocked.push(false);
    }
    g.component.resize(nb + eps, -1);
    g.queue.resize(nb + eps, 0);
    g.comp_visit.resize(nb + eps, 0);
    g.comp_group.resize(nb + eps, 0);
    g.comp_next.resize(nb + eps, -1);
    for e in 0..eps {
        let c = g.ep_cluster[e];
        nav_mark_pending(g, c);
    }
    g.comp_full = true;
    if g.phase == NAV_PHASE_COMPONENTS {
        nav_components_begin(g);
    }
}

pub(super) fn nav_link_endpoint(g: &mut NavGraph, p: Place) {
    let si = nav_space_index(g, p.space);
    let c = nav_tile_cluster(g, si, p.grid.x, p.grid.y);
    if c < 0 {
        panic!(
            "nav link tile {},{} is outside space {}",
            p.grid.x, p.grid.y, p.space.value
        );
    }
    g.ep_cluster.push(c);
    g.node_x.push(g.space_ox[si] + p.grid.x);
    g.node_y.push(g.space_oy[si] + p.grid.y);
}

pub(super) fn nav_space_index(g: &NavGraph, space: SpaceId) -> int {
    let si = g.id_space.get(&space.value).copied().unwrap_or(-1);
    if si < 0 {
        panic!("nav graph has no space {}", space.value);
    }
    si
}

pub(super) fn nav_graph_holds(g: &NavGraph, place: Place) -> bool {
    if !g.id_space.contains_key(&place.space.value) {
        return false;
    }
    place.grid.x >= 0 && place.grid.y >= 0 && place.grid.x < SPACE_SIZE && place.grid.y < SPACE_SIZE
}

pub(super) fn nav_graph_world_tile(g: &NavGraph, place: Place) -> IVec2 {
    let si = g.id_space.get(&place.space.value).copied().unwrap_or(-1);
    if si < 0 {
        return place.grid;
    }
    iv(g.space_ox[si] + place.grid.x, g.space_oy[si] + place.grid.y)
}

pub(super) fn nav_tile_index(g: &NavGraph, si: int, x: int, y: int) -> int {
    g.space_tile_base[si] + y * SPACE_SIZE + x
}

pub(super) fn nav_world_tile_index(g: &NavGraph, wx: int, wy: int, hint: int) -> Option<int> {
    let si = nav_world_space(g, wx, wy, hint);
    (si >= 0).then(|| nav_tile_index(g, si, wx - g.space_ox[si], wy - g.space_oy[si]))
}

pub(super) fn nav_tile_blocked(g: &NavGraph, si: int, x: int, y: int) -> bool {
    g.blocked[nav_tile_index(g, si, x, y)]
}

pub(super) fn nav_world_space(g: &NavGraph, wx: int, wy: int, hint: int) -> int {
    if hint >= 0 {
        let x = wx - g.space_ox[hint];
        let y = wy - g.space_oy[hint];
        if x >= 0 && y >= 0 && x < SPACE_SIZE && y < SPACE_SIZE {
            return hint;
        }
    }
    g.cell_space
        .get(&nav_grid_key(
            wx.div_euclid(SPACE_SIZE),
            wy.div_euclid(SPACE_SIZE),
        ))
        .copied()
        .unwrap_or(-1)
}

pub(super) fn nav_tile_extra(g: &NavGraph, si: int, x: int, y: int) -> int {
    g.extra[nav_tile_index(g, si, x, y)]
}

pub(super) fn nav_world_extra(g: &NavGraph, wx: int, wy: int, hint: int) -> int {
    nav_world_tile_index(g, wx, wy, hint).map_or(0, |t| g.extra[t])
}

pub(super) fn nav_tile_elbow_clear(g: &NavGraph, si: int, x: int, y: int, corner: int) -> bool {
    let t = nav_tile_index(g, si, x, y);
    diagonal_elbow_clear(g.blocked[t], g.corners[t], corner)
}

pub(super) fn nav_world_blocked(g: &NavGraph, wx: int, wy: int, hint: int) -> bool {
    nav_world_tile_index(g, wx, wy, hint).is_none_or(|t| g.blocked[t])
}

pub(super) fn nav_world_elbow_clear(
    g: &NavGraph,
    wx: int,
    wy: int,
    corner: int,
    hint: int,
) -> bool {
    nav_world_tile_index(g, wx, wy, hint)
        .is_some_and(|t| diagonal_elbow_clear(g.blocked[t], g.corners[t], corner))
}

pub(super) fn nav_world_step_open(
    g: &NavGraph,
    x: int,
    y: int,
    nx: int,
    ny: int,
    hint: int,
) -> bool {
    if nav_world_blocked(g, nx, ny, hint) {
        return false;
    }
    if nx == x || ny == y {
        return true;
    }
    let sx = nx - x;
    let sy = ny - y;
    nav_world_elbow_clear(g, nx, y, diagonal_step_corner(sx, sy, true), hint)
        && nav_world_elbow_clear(g, x, ny, diagonal_step_corner(sx, sy, false), hint)
}

pub(super) fn nav_world_diagonal_crossing(
    g: &NavGraph,
    x: int,
    y: int,
    nx: int,
    ny: int,
    hint: int,
) -> bool {
    !nav_world_blocked(g, x, y, hint)
        && nav_world_blocked(g, nx, y, hint)
        && nav_world_blocked(g, x, ny, hint)
        && nav_world_step_open(g, x, y, nx, ny, hint)
}

pub(super) fn nav_graph_set_tile(
    g: &mut NavGraph,
    space: SpaceId,
    grid: IVec2,
    blocked: bool,
    open_corners: int,
    extra: int,
) {
    let si = nav_space_index(g, space);
    if grid.x < 0 || grid.y < 0 || grid.x >= SPACE_SIZE || grid.y >= SPACE_SIZE {
        panic!(
            "nav tile {},{} is outside space {}",
            grid.x, grid.y, space.value
        );
    }
    let t = nav_tile_index(g, si, grid.x, grid.y);
    g.search_cluster = -1;
    g.blocked[t] = blocked;
    g.corners[t] = open_corners;
    g.extra[t] = extra;
}

pub(super) fn nav_mark_pending(g: &mut NavGraph, c: int) {
    if !g.pending[c] {
        g.pending[c] = true;
        g.pending_list.push(c);
    }
}

pub(super) fn nav_graph_invalidate(g: &mut NavGraph, space: SpaceId, rect: IRect) {
    let si = nav_space_index(g, space);
    let x0 = rect.x.max(0);
    let y0 = rect.y.max(0);
    let x1 = (rect.x + rect.w).min(SPACE_SIZE) - 1;
    let y1 = (rect.y + rect.h).min(SPACE_SIZE) - 1;
    if x1 < x0 || y1 < y0 {
        return;
    }
    let s = g.cluster_size;
    let cbase = g.space_cluster_base[si];
    for cy in y0.div_euclid(s)..y1.div_euclid(s) + 1 {
        for cx in x0.div_euclid(s)..x1.div_euclid(s) + 1 {
            nav_mark_pending(g, cbase + cy * g.span + cx);
        }
    }
}

pub(super) fn nav_graph_refresh_pending(g: &NavGraph) -> bool {
    g.phase != NAV_PHASE_IDLE || g.pending_list.len() > 0
}

pub(super) fn nav_graph_refresh(g: &mut NavGraph, budget: int) -> bool {
    g.refresh_spent = 0;
    loop {
        if g.phase == NAV_PHASE_IDLE {
            if g.pending_list.len() == 0 {
                return true;
            }
            if g.refresh_spent >= budget {
                return false;
            }
            nav_round_begin(g);
        } else if g.phase != NAV_PHASE_SEARCH && g.phase != NAV_PHASE_COMPONENTS {
            let (len, next) = match g.phase {
                NAV_PHASE_BEGIN => (g.pending_list.len(), NAV_PHASE_BORDERS),
                NAV_PHASE_BORDERS => (g.round_border_count, NAV_PHASE_VERTICES),
                _ => (g.round_vertex_count, NAV_PHASE_SEARCH),
            };
            if g.cursor >= len {
                if g.phase == NAV_PHASE_BEGIN {
                    g.pending_list.clear();
                }
                g.phase = next;
                g.cursor = 0;
                g.cursor_slot = 0;
                continue;
            }
            if g.refresh_spent >= budget {
                return false;
            }
            let item = g.cursor;
            g.cursor += 1;
            g.refresh_spent += match g.phase {
                NAV_PHASE_BEGIN => {
                    nav_round_take(g, g.pending_list[item]);
                    1
                }
                NAV_PHASE_BORDERS => nav_border_rebuild(g, g.round_borders[item]),
                _ => nav_vertex_rebuild(g, g.round_vertices[item]),
            };
        } else if g.phase == NAV_PHASE_SEARCH {
            if g.cursor >= g.round_clusters.len() {
                g.phase = NAV_PHASE_COMPONENTS;
                nav_components_begin(g);
                continue;
            }
            let c = g.round_clusters[g.cursor];
            let i = g.cursor_slot;
            if i >= g.k + g.cluster_ep_count[c] {
                g.round_full[c] = false;
                for dir in 0..4 {
                    g.round_dir[c * 4 + dir] = false;
                    g.round_corner[c * 4 + dir] = false;
                }
                g.cursor += 1;
                g.cursor_slot = 0;
                continue;
            }
            let border_slots = 4 * g.per_side;
            let need = g.round_full[c]
                || (i < border_slots && g.round_dir[c * 4 + i.div_euclid(g.per_side)])
                || (i >= border_slots && i < g.k && g.round_corner[c * 4 + i - border_slots]);
            if !need || nav_cluster_node(g, c, i) < 0 {
                g.cursor_slot += 1;
                continue;
            }
            if g.refresh_spent >= budget {
                return false;
            }
            g.refresh_spent += nav_cluster_search(g, c, i);
            g.cursor_slot += 1;
        } else {
            if g.refresh_spent >= budget {
                return false;
            }
            if nav_components_step(g, budget) {
                g.phase = NAV_PHASE_IDLE;
            }
        }
    }
}

pub(super) fn nav_round_begin(g: &mut NavGraph) {
    g.round_id += 1;
    g.epoch += 1;
    g.round_border_count = 0;
    g.round_vertex_count = 0;
    g.round_clusters.clear();
    g.phase = NAV_PHASE_BEGIN;
    g.cursor = 0;
}

pub(super) fn nav_round_take(g: &mut NavGraph, c: int) {
    g.pending[c] = false;
    g.round_full[c] = true;
    nav_round_add(g, c);
    g.cluster_version[c] += 1;
    for dir in 0..4 {
        let b = g.cluster_border[c * 4 + dir];
        if b < 0 {
            continue;
        }
        let other = if g.border_a[b] == c {
            g.border_b[b]
        } else {
            g.border_a[b]
        };
        g.cluster_version[other] += 1;
        if g.border_stamp[b] == g.round_id {
            continue;
        }
        g.border_stamp[b] = g.round_id;
        g.round_borders[g.round_border_count] = b;
        g.round_border_count += 1;
    }
    nav_round_add_vertex(g, g.cluster_vertex[c * 4]);
    nav_round_add_vertex(g, g.cluster_vertex[c * 4 + 1]);
    nav_round_add_vertex(g, g.cluster_vertex[c * 4 + 2]);
    nav_round_add_vertex(g, g.cluster_vertex[c * 4 + 3]);
}

pub(super) fn nav_round_add_vertex(g: &mut NavGraph, v: int) {
    if v >= 0 && g.vertex_stamp[v] != g.round_id {
        g.vertex_stamp[v] = g.round_id;
        g.round_vertices[g.round_vertex_count] = v;
        g.round_vertex_count += 1;
    }
}

pub(super) fn nav_round_add(g: &mut NavGraph, c: int) {
    if g.round_stamp[c] != g.round_id {
        g.round_stamp[c] = g.round_id;
        g.round_clusters.push(c);
    }
}

pub(super) fn nav_place_entrance(
    g: &mut NavGraph,
    b: int,
    vertical: bool,
    wx: int,
    wy: int,
    shift: int,
    lo: int,
    hi: int,
) {
    let slot = g.border_count[b];
    if slot >= g.per_side {
        panic!("nav border {} exceeds {} entrances", b, g.per_side);
    }
    let n0 = b * 2 * g.per_side + slot;
    let n1 = n0 + g.per_side;
    g.node_x[n0] = wx;
    g.node_y[n0] = wy;
    g.node_x[n1] = if vertical { wx + 1 } else { wx + shift };
    g.node_y[n1] = if vertical { wy + shift } else { wy + 1 };
    g.gap_lo[n0] = lo;
    g.gap_hi[n0] = hi;
    g.gap_lo[n1] = lo;
    g.gap_hi[n1] = hi;
    g.border_count[b] = slot + 1;
}

/// Forgets every cost from slot `i` of cluster `c`; returns the work charged.
fn nav_clear_slot(g: &mut NavGraph, c: int, i: int) -> int {
    for j in 0..g.k {
        let at = nav_cost_index(g, c, i, j);
        g.cost[at] = -1;
    }
    for q in 0..g.cluster_ep_count[c] {
        let endpoint = g.cluster_eps[g.cluster_ep_start[c] + q];
        g.ep_cost[g.ep_row[endpoint] + i] = -1;
    }
    g.k + g.cluster_ep_count[c]
}

pub(super) fn nav_clear_dir(g: &mut NavGraph, c: int, dir: int, count: int) -> int {
    g.round_dir[c * 4 + dir] = true;
    (0..count)
        .map(|slot| nav_clear_slot(g, c, dir * g.per_side + slot))
        .sum()
}

pub(super) fn nav_border_rebuild(g: &mut NavGraph, b: int) -> int {
    let a = g.border_a[b];
    let bc = g.border_b[b];
    let sa = g.cluster_space[a];
    let sb = g.cluster_space[bc];
    let vertical = g.border_vertical[b];
    let len = g.cluster_size;
    let ax = if vertical {
        g.cluster_x[a] + len - 1
    } else {
        g.cluster_x[a]
    };
    let ay = if vertical {
        g.cluster_y[a]
    } else {
        g.cluster_y[a] + len - 1
    };
    let bx = g.cluster_x[bc];
    let by = g.cluster_y[bc];
    let dx = if vertical { 0 } else { 1 };
    let dy = if vertical { 1 } else { 0 };
    let wx = g.cluster_wx[a] + ax - g.cluster_x[a];
    let wy = g.cluster_wy[a] + ay - g.cluster_y[a];
    let old_count = g.border_count[b];
    let first = b * 2 * g.per_side;
    let entrance = |g: &NavGraph, i: int| {
        let (n0, n1) = (first + i, first + g.per_side + i);
        [g.node_x[n0], g.node_y[n0], g.node_x[n1], g.node_y[n1]]
    };
    for i in 0..old_count {
        g.border_old[i] = entrance(g, i);
    }
    g.border_count[b] = 0;
    let mut span_start = -1;
    for p in 0..(len + 1) {
        let open = p < len
            && !nav_tile_blocked(g, sa, ax + p * dx, ay + p * dy)
            && !nav_tile_blocked(g, sb, bx + p * dx, by + p * dy);
        if open {
            if span_start < 0 {
                span_start = p;
            }
        } else if span_start >= 0 {
            let span_end = p - 1;
            let span = span_end - span_start + 1;
            if span < NAV_LONG_SPAN {
                let mid = span_start + (span - 1).div_euclid(2);
                nav_place_entrance(
                    g,
                    b,
                    vertical,
                    wx + mid * dx,
                    wy + mid * dy,
                    0,
                    span_start - mid,
                    span_end - mid,
                );
            } else {
                nav_place_entrance(
                    g,
                    b,
                    vertical,
                    wx + span_start * dx,
                    wy + span_start * dy,
                    0,
                    0,
                    span_end - span_start,
                );
                nav_place_entrance(
                    g,
                    b,
                    vertical,
                    wx + span_end * dx,
                    wy + span_end * dy,
                    0,
                    span_start - span_end,
                    0,
                );
            }
            span_start = -1;
        }
    }
    let px = if vertical { 1 } else { 0 };
    let py = if vertical { 0 } else { 1 };
    for p in 0..(len - 1) {
        let a0 = nav_tile_blocked(g, sa, ax + p * dx, ay + p * dy);
        let a1 = nav_tile_blocked(g, sa, ax + (p + 1) * dx, ay + (p + 1) * dy);
        let b0 = nav_tile_blocked(g, sb, bx + p * dx, by + p * dy);
        let b1 = nav_tile_blocked(g, sb, bx + (p + 1) * dx, by + (p + 1) * dy);
        let x0 = wx + p * dx;
        let y0 = wy + p * dy;
        if !a0
            && a1
            && b0
            && !b1
            && nav_world_diagonal_crossing(g, x0, y0, x0 + px + dx, y0 + py + dy, sa)
        {
            nav_place_entrance(g, b, vertical, x0, y0, 1, 0, 0);
        } else if a0
            && !a1
            && !b0
            && b1
            && nav_world_diagonal_crossing(g, x0 + dx, y0 + dy, x0 + px, y0 + py, sa)
        {
            nav_place_entrance(g, b, vertical, x0 + dx, y0 + dy, -1, 0, 0);
        }
    }
    let mut work = 2 * len;
    let moved =
        g.border_count[b] != old_count || (0..old_count).any(|i| entrance(g, i) != g.border_old[i]);
    if !moved {
        return work;
    }
    nav_round_add(g, a);
    nav_round_add(g, bc);
    if vertical {
        work += nav_clear_dir(g, a, 1, old_count);
        work += nav_clear_dir(g, bc, 3, old_count);
    } else {
        work += nav_clear_dir(g, a, 2, old_count);
        work += nav_clear_dir(g, bc, 0, old_count);
    }
    work
}

pub(super) fn nav_clear_corner(g: &mut NavGraph, c: int, q: int) -> int {
    g.round_corner[c * 4 + q] = true;
    nav_round_add(g, c);
    g.cluster_version[c] += 1;
    nav_clear_slot(g, c, 4 * g.per_side + q)
}

pub(super) fn nav_vertex_rebuild(g: &mut NavGraph, v: int) -> int {
    let x = g.vertex_x[v];
    let y = g.vertex_y[v];
    let hint = g.cluster_space[g.vertex_cluster[v * 4]];
    let a_live = nav_world_diagonal_crossing(g, x - 1, y - 1, x, y, hint);
    let a = nav_vertex_diagonal(g, v, 2, a_live);
    let b_live = nav_world_diagonal_crossing(g, x, y - 1, x - 1, y, hint);
    let b = nav_vertex_diagonal(g, v, 3, b_live);
    2 + a + b
}

pub(super) fn nav_vertex_diagonal(g: &mut NavGraph, v: int, q: int, live: bool) -> int {
    if live == g.corner_live[v * 4 + q] {
        return 0;
    }
    g.corner_live[v * 4 + q] = live;
    g.corner_live[v * 4 + q - 2] = live;
    let a = nav_clear_corner(g, g.vertex_cluster[v * 4 + q], q);
    let b = nav_clear_corner(g, g.vertex_cluster[v * 4 + q - 2], q - 2);
    a + b
}

#[inline]
fn nav_cost_index(g: &NavGraph, c: int, i: int, j: int) -> int {
    let hi = max(i, j);
    c * g.k * (g.k + 1) / 2 + hi * (hi + 1) / 2 + min(i, j)
}

pub(super) fn nav_slot_set(g: &mut NavGraph, c: int, i: int, j: int, cost: int) {
    let k = g.k;
    if i < k && j < k {
        let at = nav_cost_index(g, c, i, j);
        g.cost[at] = cost;
        return;
    }
    let start = g.cluster_ep_start[c];
    if i >= k {
        let ep = g.cluster_eps[start + i - k];
        g.ep_cost[g.ep_row[ep] + j] = cost;
    }
    if j >= k {
        let ep = g.cluster_eps[start + j - k];
        g.ep_cost[g.ep_row[ep] + i] = cost;
    }
}

pub(super) fn nav_intra_cost(g: &NavGraph, c: int, i: int, j: int) -> int {
    let k = g.k;
    if i < k && j < k {
        return g.cost[nav_cost_index(g, c, i, j)];
    }
    let start = g.cluster_ep_start[c];
    if i >= k {
        let ep = g.cluster_eps[start + i - k];
        return g.ep_cost[g.ep_row[ep] + j];
    }
    let ep = g.cluster_eps[start + j - k];
    g.ep_cost[g.ep_row[ep] + i]
}

fn nav_cluster_prepare(g: &mut NavGraph, c: int) {
    if g.search_cluster == c {
        return;
    }
    let si = g.cluster_space[c];
    let ox = g.cluster_x[c];
    let oy = g.cluster_y[c];
    let s = g.cluster_size;
    let tbase = nav_tile_index(g, si, ox, oy);
    let uniform = g.extra[tbase];
    g.search_uniform = if uniform >= 0
        && (0..s).all(|y| {
            (0..s).all(|x| {
                let tile = tbase + y * SPACE_SIZE + x;
                !g.blocked[tile] && g.extra[tile] == uniform
            })
        }) {
        uniform
    } else {
        -1
    };
    g.search_ready.0.fill(false);
    g.search_cluster = c;
}

/// The cost of stepping from local tile `(x, y)` of cluster `c` by `(dx, dy)`,
/// or `None` when a diagonal step's elbow tiles are closed.
fn nav_cluster_step_cost(g: &NavGraph, c: int, x: int, y: int, dx: int, dy: int) -> Option<int> {
    let si = g.cluster_space[c];
    let ox = g.cluster_x[c];
    let oy = g.cluster_y[c];
    let mut step = NAV_COST_STRAIGHT;
    if dx != 0 && dy != 0 {
        if !nav_tile_elbow_clear(
            g,
            si,
            ox + x + dx,
            oy + y,
            diagonal_step_corner(dx, dy, true),
        ) || !nav_tile_elbow_clear(
            g,
            si,
            ox + x,
            oy + y + dy,
            diagonal_step_corner(dx, dy, false),
        ) {
            return None;
        }
        step = NAV_COST_DIAGONAL;
    }
    Some(nav_edge_cost(
        step,
        nav_tile_extra(g, si, ox + x, oy + y),
        nav_tile_extra(g, si, ox + x + dx, oy + y + dy),
    ))
}

/// Calls `f` with each in-cluster neighbour of local tile `t` and its offset.
fn nav_cluster_neighbours(s: int, t: int, mut f: impl FnMut(int, int, int)) {
    let (x, y) = (t % s, t / s);
    for dy in -1..2 {
        for dx in -1..2 {
            let (nx, ny) = (x + dx, y + dy);
            if (dx != 0 || dy != 0) && nx >= 0 && nx < s && ny >= 0 && ny < s {
                f(dx, dy, ny * s + nx);
            }
        }
    }
}

fn nav_cluster_prepare_steps(g: &mut NavGraph, c: int, tile: int) {
    if g.search_ready[tile] {
        return;
    }
    let s = g.cluster_size;
    let (si, ox, oy) = (g.cluster_space[c], g.cluster_x[c], g.cluster_y[c]);
    for edge in 0..9 {
        g.search_edges[tile * 9 + edge] = None;
    }
    nav_cluster_neighbours(s, tile, |dx, dy, nt| {
        if !nav_tile_blocked(g, si, ox + nt % s, oy + nt / s) {
            g.search_edges[tile * 9 + (dy + 1) * 3 + dx + 1] =
                nav_cluster_step_cost(g, c, tile % s, tile / s, dx, dy);
        }
    });
    g.search_ready[tile] = true;
}

pub(super) fn nav_cluster_search(g: &mut NavGraph, c: int, from: int) -> int {
    nav_cluster_prepare(g, c);
    let si = g.cluster_space[c];
    let ox = g.cluster_x[c];
    let oy = g.cluster_y[c];
    let wox = g.cluster_wx[c];
    let woy = g.cluster_wy[c];
    let s = g.cluster_size;
    let limit = g.k + g.cluster_ep_count[c];
    g.window.begin();
    let src = nav_cluster_node(g, c, from);
    let start = (g.node_y[src] - woy) * s + (g.node_x[src] - wox);
    let mut expanded = 0;
    let start_blocked = nav_tile_blocked(g, si, ox + start % s, oy + start / s);
    if src >= g.ep_base {
        g.ep_blocked[src - g.ep_base] = start_blocked;
    }
    let uniform = g.search_uniform;
    if uniform >= 0 {
        let straight = nav_edge_cost(NAV_COST_STRAIGHT, uniform, uniform);
        let diagonal = nav_edge_cost(NAV_COST_DIAGONAL, uniform, uniform);
        for j in 0..limit {
            let n = nav_cluster_node(g, c, j);
            if n < 0 {
                continue;
            }
            let dx = (g.node_x[n] - g.node_x[src]).abs();
            let dy = (g.node_y[n] - g.node_y[src]).abs();
            let cost = min(dx, dy) * diagonal + (max(dx, dy) - min(dx, dy)) * straight;
            nav_slot_set(g, c, from, j, cost);
        }
        return s * s + 1;
    }
    if !start_blocked || (src >= g.ep_base && !nav_node_is_pad(g, src)) {
        g.window.seed(start);
        while let Some((t, d)) = g.window.pop() {
            expanded += 1;
            nav_cluster_prepare_steps(g, c, t);
            for edge in 0..9 {
                if let Some(cost) = g.search_edges[t * 9 + edge] {
                    g.window
                        .relax(t, t + (edge / 3 - 1) * s + edge % 3 - 1, d + cost);
                }
            }
        }
    }
    for j in 0..limit {
        let n = nav_cluster_node(g, c, j);
        if n < 0 {
            continue;
        }
        let t = (g.node_y[n] - woy) * s + (g.node_x[n] - wox);
        let mut cost = -1;
        if g.window.reached(t) {
            cost = g.window.dist[t];
        } else if n >= g.ep_base
            && !nav_node_is_pad(g, n)
            && nav_tile_blocked(g, si, ox + t % s, oy + t / s)
        {
            cost = nav_exit_cost(g, c, t);
        }
        nav_slot_set(g, c, from, j, cost);
    }
    expanded + 1
}

/// The cheapest step onto blocked local tile `t` from a tile the last search reached.
pub(super) fn nav_exit_cost(g: &NavGraph, c: int, t: int) -> int {
    let s = g.cluster_size;
    let mut best = -1;
    nav_cluster_neighbours(s, t, |dx, dy, nt| {
        if g.window.reached(nt) {
            if let Some(step) = nav_cluster_step_cost(g, c, t % s, t / s, dx, dy) {
                let cost = g.window.dist[nt] + step;
                if best < 0 || cost < best {
                    best = cost;
                }
            }
        }
    });
    best
}

pub(super) fn nav_node_blocked_endpoint(g: &NavGraph, n: int) -> bool {
    n >= g.ep_base && g.ep_blocked[n - g.ep_base]
}

pub(super) fn nav_components_begin(g: &mut NavGraph) {
    if g.comp_stamp >= NAV_STAMP_LIMIT {
        for i in 0..g.comp_visit.len() {
            g.comp_visit[i] = 0;
        }
        for i in 0..g.label_stamp.len() {
            g.label_stamp[i] = 0;
        }
        g.comp_stamp = 0;
    }
    g.comp_stamp += 1;
    g.comp_stage = NAV_COMP_DETACH;
    g.comp_cursor = 0;
    g.comp_head = 0;
    g.comp_tail = 0;
    g.comp_seeds = 0;
    g.flood_count = 0;
    g.comp_unfinished = 0;
    g.comp_merge = false;
    g.comp_touched.clear();
    g.claim_count = 0;
    g.comp_check_active = false;
    if g.comp_full {
        g.label_size.clear();
        g.label_stamp.clear();
        g.label_owner.clear();
        g.label_owner_stamp.clear();
        g.label_free.clear();
    }
}

pub(super) fn nav_components_step(g: &mut NavGraph, budget: int) -> bool {
    while g.refresh_spent < budget {
        let stage = g.comp_stage;
        if stage == NAV_COMP_DETACH {
            let count = if g.comp_full {
                g.cluster_ep_count.len()
            } else {
                g.round_clusters.len()
            };
            if g.comp_cursor < count {
                let c = if g.comp_full {
                    g.comp_cursor
                } else {
                    g.round_clusters[g.comp_cursor]
                };
                g.refresh_spent += nav_components_detach(g, c);
                g.comp_cursor += 1;
            } else {
                g.comp_stage = NAV_COMP_SEEDS;
                g.comp_seeds = g.comp_tail;
            }
        } else if stage == NAV_COMP_SEEDS {
            if g.comp_unfinished <= 1 {
                g.comp_stage = NAV_COMP_KEEP;
                g.comp_cursor = 0;
            } else if g.comp_head < g.comp_seeds {
                let u = g.queue[g.comp_head];
                nav_components_expand(g, u);
                g.comp_head += 1;
                g.refresh_spent += 1;
            } else {
                g.comp_stage = NAV_COMP_RACE;
                g.comp_checked = g.comp_unfinished + 1;
                g.comp_run_at = 0;
                g.comp_run.clear();
                for f in 0..g.flood_count {
                    if nav_flood_running(g, f) {
                        g.comp_run.push(f);
                    }
                }
                g.refresh_spent += g.flood_count;
            }
        } else if stage == NAV_COMP_RACE {
            if g.comp_check_active {
                if nav_components_apart_step(g, budget) && g.comp_check_apart {
                    g.comp_stage = NAV_COMP_KEEP;
                    g.comp_cursor = 0;
                }
            } else if g.comp_unfinished <= 1 {
                g.comp_stage = NAV_COMP_KEEP;
                g.comp_cursor = 0;
            } else if g.comp_unfinished < g.comp_checked {
                g.comp_checked = g.comp_unfinished;
                g.comp_check_id += 1;
                g.comp_check_active = true;
                g.comp_check_flood = 0;
                g.comp_check_claim = -1;
                g.comp_check_apart = true;
                if nav_components_apart_step(g, budget) && g.comp_check_apart {
                    g.comp_stage = NAV_COMP_KEEP;
                    g.comp_cursor = 0;
                }
            } else if g.comp_run_at >= g.comp_run.len() {
                let mut kept = 0;
                let run_len = g.comp_run.len();
                for i in 0..run_len {
                    let f = g.comp_run[i];
                    if nav_flood_running(g, f) {
                        g.comp_run[kept] = f;
                        kept += 1;
                    }
                }
                while g.comp_run.len() > kept {
                    g.comp_run.pop();
                }
                g.comp_run_at = 0;
                g.refresh_spent += 1;
            } else {
                let f = g.comp_run[g.comp_run_at];
                g.comp_run_at += 1;
                if nav_flood_running(g, f) {
                    let u = g.flood_head[f];
                    g.flood_head[f] = g.comp_next[u];
                    if g.flood_head[f] < 0 {
                        g.flood_tail[f] = -1;
                    }
                    nav_components_expand(g, u);
                    g.refresh_spent += 1;
                }
            }
        } else if stage == NAV_COMP_KEEP {
            if g.comp_cursor < g.flood_count {
                let f = g.comp_cursor;
                g.comp_cursor += 1;
                g.refresh_spent += 1;
                if nav_flood_running(g, f) {
                    g.refresh_spent += nav_components_keep(g, f);
                }
            } else {
                g.comp_stage = NAV_COMP_LABEL;
                g.comp_cursor = 0;
            }
        } else if stage == NAV_COMP_LABEL {
            if g.comp_cursor < g.comp_tail {
                let n = g.queue[g.comp_cursor];
                let flood = g.comp_group[n];
                let r = nav_flood_root(g, flood);
                if g.flood_label[r] < 0 {
                    g.flood_label[r] = nav_label_new(g);
                }
                let label = g.flood_label[r];
                nav_label_set(g, n, label);
                g.comp_cursor += 1;
                g.refresh_spent += 1;
            } else {
                g.comp_stage = if g.comp_merge {
                    NAV_COMP_MERGE
                } else {
                    NAV_COMP_END
                };
                g.comp_cursor = 0;
            }
        } else if stage == NAV_COMP_MERGE {
            if g.comp_cursor < g.comp_tail {
                let n = g.queue[g.comp_cursor];
                let flood = g.comp_group[n];
                let r = nav_flood_root(g, flood);
                if g.flood_open[r] > 0 {
                    nav_components_absorb(g, n, r);
                }
                g.comp_cursor += 1;
                g.refresh_spent += 1;
            } else {
                g.comp_stage = NAV_COMP_END;
            }
        } else {
            for i in 0..g.comp_touched.len() {
                let label = g.comp_touched[i];
                if g.label_size[label] == 0 {
                    g.label_free.push(label);
                }
            }
            g.refresh_spent += g.comp_touched.len() + 1;
            g.comp_full = false;
            return true;
        }
    }
    false
}

pub(super) fn nav_cluster_slot_node(g: &NavGraph, c: int, i: int) -> int {
    if i >= g.k {
        return g.ep_base + g.cluster_eps[g.cluster_ep_start[c] + i - g.k];
    }
    if i >= 4 * g.per_side {
        let q = i - 4 * g.per_side;
        let v = g.cluster_vertex[c * 4 + q];
        return if v < 0 { -1 } else { g.corner_base + v * 4 + q };
    }
    let dir = i.div_euclid(g.per_side);
    let b = g.cluster_border[c * 4 + dir];
    if b < 0 {
        return -1;
    }
    let side = if dir == 0 || dir == 3 { 1 } else { 0 };
    b * 2 * g.per_side + side * g.per_side + i % g.per_side
}

pub(super) fn nav_components_detach(g: &mut NavGraph, c: int) -> int {
    let limit = g.k + g.cluster_ep_count[c];
    for i in 0..limit {
        let n = nav_cluster_slot_node(g, c, i);
        if n >= 0 {
            let old = if g.comp_full { -1 } else { g.component[n] };
            if old >= 0 {
                g.label_size[old] -= 1;
                nav_components_touch(g, old);
            }
            g.component[n] = -1;
            if nav_node_live(g, n) {
                let f = g.flood_count;
                nav_put(&mut g.flood_parent, f, f);
                nav_put(&mut g.flood_open, f, 1);
                nav_put(&mut g.flood_label, f, -1);
                nav_put(&mut g.flood_head, f, -1);
                nav_put(&mut g.flood_tail, f, -1);
                nav_put(&mut g.flood_claims, f, -1);
                g.flood_count = f + 1;
                g.comp_unfinished += 1;
                g.comp_visit[n] = g.comp_stamp;
                g.comp_group[n] = f;
                g.queue[g.comp_tail] = n;
                g.comp_tail += 1;
                if old >= 0 {
                    nav_components_claim(g, old, f);
                }
            }
        }
    }
    limit + 1
}

pub(super) fn nav_put(v: &mut IdxVec<int>, i: int, value: int) {
    if i < v.len() {
        v[i] = value;
    } else {
        v.push(value);
    }
}

pub(super) fn nav_flood_root(g: &mut NavGraph, f: int) -> int {
    let mut r = f;
    while g.flood_parent[r] != r {
        r = g.flood_parent[r];
    }
    let mut x = f;
    while g.flood_parent[x] != r {
        let next = g.flood_parent[x];
        g.flood_parent[x] = r;
        x = next;
    }
    r
}

pub(super) fn nav_label_new(g: &mut NavGraph) -> int {
    let n = g.label_free.len();
    if n > 0 {
        let l = g.label_free[n - 1];
        g.label_free.pop();
        return l;
    }
    g.label_size.push(0);
    g.label_stamp.push(0);
    g.label_owner.push(-1);
    g.label_owner_stamp.push(0);
    g.label_size.len() - 1
}

pub(super) fn nav_label_set(g: &mut NavGraph, n: int, label: int) {
    let old = g.component[n];
    if old >= 0 {
        g.label_size[old] -= 1;
    }
    g.component[n] = label;
    g.label_size[label] += 1;
}

pub(super) fn nav_components_touch(g: &mut NavGraph, l: int) {
    if g.label_stamp[l] != g.comp_stamp {
        g.label_stamp[l] = g.comp_stamp;
        g.comp_touched.push(l);
    }
}

pub(super) fn nav_flood_running(g: &NavGraph, f: int) -> bool {
    g.flood_parent[f] == f && g.flood_open[f] > 0
}

pub(super) fn nav_components_claim(g: &mut NavGraph, l: int, f: int) {
    nav_components_touch(g, l);
    let mut e = g.flood_claims[f];
    while e >= 0 {
        if g.claim_label[e] == l {
            return;
        }
        e = g.claim_next[e];
    }
    let c = g.claim_count;
    nav_put(&mut g.claim_label, c, l);
    let next = g.flood_claims[f];
    nav_put(&mut g.claim_next, c, next);
    g.claim_count = c + 1;
    g.flood_claims[f] = c;
}

pub(super) fn nav_components_keep(g: &mut NavGraph, f: int) -> int {
    let mut walked = 0;
    let mut e = g.flood_claims[f];
    while e >= 0 {
        let l = g.claim_label[e];
        let keep = g.flood_label[f];
        if g.label_size[l] > 0 && (keep < 0 || g.label_size[l] > g.label_size[keep]) {
            g.flood_label[f] = l;
        }
        walked += 1;
        e = g.claim_next[e];
    }
    e = g.flood_claims[f];
    while e >= 0 {
        let l = g.claim_label[e];
        if g.label_size[l] > 0 && l != g.flood_label[f] {
            g.comp_merge = true;
        }
        e = g.claim_next[e];
    }
    2 * walked
}

pub(super) fn nav_components_apart_step(g: &mut NavGraph, budget: int) -> bool {
    let id = g.comp_check_id;
    while g.comp_check_flood < g.flood_count && g.refresh_spent < budget {
        let f = g.comp_check_flood;
        if g.comp_check_claim < 0 {
            g.refresh_spent += 1;
            if nav_flood_running(g, f) {
                g.comp_check_claim = g.flood_claims[f];
            }
            if g.comp_check_claim < 0 {
                g.comp_check_flood += 1;
            }
            continue;
        }

        let e = g.comp_check_claim;
        let l = g.claim_label[e];
        g.refresh_spent += 1;
        if g.label_owner_stamp[l] == id && g.label_owner[l] != f {
            g.comp_check_apart = false;
            g.comp_check_active = false;
            return true;
        }
        g.label_owner_stamp[l] = id;
        g.label_owner[l] = f;
        g.comp_check_claim = g.claim_next[e];
        if g.comp_check_claim < 0 {
            g.comp_check_flood += 1;
        }
    }
    if g.comp_check_flood < g.flood_count {
        return false;
    }
    g.comp_check_active = false;
    true
}

pub(super) fn nav_components_expand(g: &mut NavGraph, u: int) {
    let group = g.comp_group[u];
    let mut f = nav_flood_root(g, group);
    let mut p = nav_node_partner(g, u);
    if p < 0 {
        p = nav_node_link_other(g, u);
    }
    f = nav_components_meet(g, f, p);
    let c = nav_node_cluster(g, u);
    let i = nav_node_slot(g, u);
    for j in 0..(g.k + g.cluster_ep_count[c]) {
        if nav_intra_cost(g, c, i, j) >= 0 {
            let v = nav_cluster_node(g, c, j);
            if v >= 0 {
                f = nav_components_meet(g, f, v);
            }
        }
    }
    g.flood_open[f] -= 1;
    if g.flood_open[f] == 0 {
        g.comp_unfinished -= 1;
    }
}

pub(super) fn nav_components_meet(g: &mut NavGraph, f: int, v: int) -> int {
    if g.comp_visit[v] != g.comp_stamp {
        g.comp_visit[v] = g.comp_stamp;
        g.comp_group[v] = f;
        g.flood_open[f] += 1;
        g.queue[g.comp_tail] = v;
        g.comp_tail += 1;
        g.comp_next[v] = -1;
        if g.flood_tail[f] >= 0 {
            let tail = g.flood_tail[f];
            g.comp_next[tail] = v;
        } else {
            g.flood_head[f] = v;
        }
        g.flood_tail[f] = v;
        let old = g.component[v];
        if old >= 0 {
            nav_components_claim(g, old, f);
        }
        return f;
    }
    let group = g.comp_group[v];
    let o = nav_flood_root(g, group);
    if o == f {
        return f;
    }
    let root = f.min(o);
    let other = f.max(o);
    g.flood_parent[other] = root;
    g.flood_open[root] += g.flood_open[other];
    if g.flood_head[other] >= 0 {
        if g.flood_tail[root] >= 0 {
            let tail = g.flood_tail[root];
            let other_head = g.flood_head[other];
            g.comp_next[tail] = other_head;
        } else {
            g.flood_head[root] = g.flood_head[other];
        }
        g.flood_tail[root] = g.flood_tail[other];
    }
    let mut e = g.flood_claims[other];
    if e >= 0 {
        while g.claim_next[e] >= 0 {
            e = g.claim_next[e];
        }
        let claims = g.flood_claims[root];
        g.claim_next[e] = claims;
        g.flood_claims[root] = g.flood_claims[other];
    }
    g.comp_unfinished -= 1;
    root
}

pub(super) fn nav_components_absorb(g: &mut NavGraph, n: int, r: int) {
    let mut p = nav_node_partner(g, n);
    if p < 0 {
        p = nav_node_link_other(g, n);
    }
    nav_components_take(g, p, r);
    let c = nav_node_cluster(g, n);
    let i = nav_node_slot(g, n);
    for j in 0..(g.k + g.cluster_ep_count[c]) {
        if nav_intra_cost(g, c, i, j) >= 0 {
            let v = nav_cluster_node(g, c, j);
            if v >= 0 {
                nav_components_take(g, v, r);
            }
        }
    }
}

pub(super) fn nav_components_take(g: &mut NavGraph, v: int, r: int) {
    if g.comp_visit[v] == g.comp_stamp || g.component[v] == g.flood_label[r] {
        return;
    }
    g.comp_visit[v] = g.comp_stamp;
    g.comp_group[v] = r;
    let label = g.flood_label[r];
    nav_label_set(g, v, label);
    g.queue[g.comp_tail] = v;
    g.comp_tail += 1;
}

pub(super) fn nav_graph_node_limit(g: &NavGraph) -> int {
    g.component.len()
}

pub(super) fn nav_node_live(g: &NavGraph, n: int) -> bool {
    if n >= g.ep_base {
        return true;
    }
    if n >= g.corner_base {
        return g.corner_live[n - g.corner_base];
    }
    let per = 2 * g.per_side;
    n % g.per_side < g.border_count[n.div_euclid(per)]
}

pub(super) fn nav_node_place(g: &NavGraph, n: int) -> Place {
    nav_world_place(
        g,
        g.cluster_space[nav_node_cluster(g, n)],
        g.node_x[n],
        g.node_y[n],
    )
}

pub(super) fn nav_world_place(g: &NavGraph, si: int, wx: int, wy: int) -> Place {
    place(g.space_id[si], wx - g.space_ox[si], wy - g.space_oy[si])
}

pub(super) fn nav_node_partner(g: &NavGraph, n: int) -> int {
    if n >= g.ep_base {
        return -1;
    }
    if n >= g.corner_base {
        let r = n - g.corner_base;
        return g.corner_base + r - r % 4 + (r % 4 + 2) % 4;
    }
    let per = 2 * g.per_side;
    if n % per < g.per_side {
        return n + g.per_side;
    }
    n - g.per_side
}

pub(super) fn nav_node_partner_cost(g: &NavGraph, n: int) -> int {
    let p = nav_node_partner(g, n);
    let extra = nav_world_extra(g, g.node_x[n], g.node_y[n], -1)
        + nav_world_extra(g, g.node_x[p], g.node_y[p], -1);
    if g.node_x[p] != g.node_x[n] && g.node_y[p] != g.node_y[n] {
        return nav_edge_cost(NAV_COST_DIAGONAL, extra, 0);
    }
    nav_edge_cost(NAV_COST_STRAIGHT, extra, 0)
}

pub(super) fn nav_node_link(g: &NavGraph, n: int) -> int {
    if n < g.ep_base {
        return -1;
    }
    (n - g.ep_base).div_euclid(2)
}

pub(super) fn nav_node_is_pad(g: &NavGraph, n: int) -> bool {
    n >= g.ep_base && (n - g.ep_base) % 2 == 0
}

pub(super) fn nav_node_link_other(g: &NavGraph, n: int) -> int {
    if n < g.ep_base {
        return -1;
    }
    if nav_node_is_pad(g, n) {
        return n + 1;
    }
    n - 1
}

pub(super) fn nav_node_cluster(g: &NavGraph, n: int) -> int {
    if n >= g.ep_base {
        return g.ep_cluster[n - g.ep_base];
    }
    if n >= g.corner_base {
        return g.vertex_cluster[n - g.corner_base];
    }
    let per = 2 * g.per_side;
    let b = n.div_euclid(per);
    if n % per < g.per_side {
        return g.border_a[b];
    }
    g.border_b[b]
}

pub(super) fn nav_node_slot(g: &NavGraph, n: int) -> int {
    if n >= g.ep_base {
        return g.k + g.ep_index[n - g.ep_base];
    }
    if n >= g.corner_base {
        return 4 * g.per_side + (n - g.corner_base) % 4;
    }
    let per = 2 * g.per_side;
    let b = n.div_euclid(per);
    let side_b = n % per >= g.per_side;
    let dir = if g.border_vertical[b] {
        if side_b {
            3
        } else {
            1
        }
    } else if side_b {
        0
    } else {
        2
    };
    dir * g.per_side + n % g.per_side
}

pub(super) fn nav_cluster_slot_limit(g: &NavGraph, c: int) -> int {
    g.k + g.cluster_ep_count[c]
}

/// The live node in slot `i` of cluster `c`, or -1.
pub(super) fn nav_cluster_node(g: &NavGraph, c: int, i: int) -> int {
    let n = nav_cluster_slot_node(g, c, i);
    if n >= 0 && nav_node_live(g, n) {
        n
    } else {
        -1
    }
}

pub(super) fn nav_cluster_pending(g: &NavGraph, c: int) -> bool {
    g.pending[c] || g.phase != NAV_PHASE_IDLE
}

pub(super) fn nav_tile_cluster(g: &NavGraph, si: int, x: int, y: int) -> int {
    if x < 0 || y < 0 || x >= SPACE_SIZE || y >= SPACE_SIZE {
        return -1;
    }
    let s = g.cluster_size;
    g.space_cluster_base[si] + y.div_euclid(s) * g.span + x.div_euclid(s)
}
