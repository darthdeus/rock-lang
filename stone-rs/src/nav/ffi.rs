//! Owned navigation handles. All pointer inputs are copied before returning.
use super::*;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

// Two signed words preserve Rock's exact integer range without exposing i64
// cdata to Lua arithmetic. Lengths count values, not words.
pub(super) use crate::native_ffi::NativeNavIntWire;

/// rock-ffi-array stone-native NAV_SPACE_LAYOUT
type SpaceLayoutRow = [NativeNavIntWire; 3];
/// rock-ffi-array stone-native NAV_LINK
type LinkRow = [NativeNavIntWire; 8];
/// rock-ffi-array stone-native NAV_ROUTE
type RouteRow = [NativeNavIntWire; 3];
/// rock-ffi-array stone-native NAV_PLACE
type PlaceRow = [NativeNavIntWire; 3];

const WORD_BYTES: usize = std::mem::size_of::<NativeNavIntWire>();
const SPACE_LANES: usize = std::mem::size_of::<SpaceLayoutRow>() / WORD_BYTES;
const LINK_LANES: usize = std::mem::size_of::<LinkRow>() / WORD_BYTES;
const ROUTE_LANES: usize = std::mem::size_of::<RouteRow>() / WORD_BYTES;

impl NativeNavIntWire {
    pub(super) fn value(self) -> int {
        ((self.hi as int) << 32) | (self.lo as u32 as int)
    }
}
impl From<int> for NativeNavIntWire {
    fn from(value: int) -> Self {
        Self {
            lo: value as i32,
            hi: (value >> 32) as i32,
        }
    }
}

struct PendingQuery {
    job: Option<Arc<QueryJob>>,
    result: Option<QueryResult>,
    charged: int,
}
struct NativeNav {
    graph: Arc<NavGraph>,
    query: NavQuery,
    jobs: BTreeMap<int, PendingQuery>,
    worker_ms: f64,
    wait_ms: f64,
    completed: int,
    area_work: NavWorkCounters,
    descend: Option<DescendSelection>,
}
impl NativeNav {
    fn fork(&self) -> Self {
        let jobs = self
            .jobs
            .iter()
            .map(|(&slot, pending)| {
                (
                    slot,
                    PendingQuery {
                        job: if pending.result.is_none() {
                            Some(QueryJob::submit(
                                Arc::clone(&self.graph),
                                self.query.plans[slot].clone(),
                            ))
                        } else {
                            None
                        },
                        result: pending.result.clone(),
                        charged: pending.charged,
                    },
                )
            })
            .collect();
        Self {
            graph: Arc::clone(&self.graph),
            query: self.query.clone(),
            jobs,
            worker_ms: self.worker_ms,
            wait_ms: self.wait_ms,
            completed: self.completed,
            area_work: self.area_work.clone(),
            descend: self.descend.clone(),
        }
    }
    fn cancel_jobs(&mut self) {
        for pending in self.jobs.values() {
            if let Some(job) = &pending.job {
                job.cancel();
            }
        }
        self.jobs.clear();
    }
    fn submit(&mut self) {
        if nav_graph_refresh_pending(&self.graph) {
            return;
        }
        let mut slot = self.query.head;
        while slot >= 0 {
            let plan = &self.query.plans[slot];
            if !self.jobs.contains_key(&slot) {
                self.jobs.insert(
                    slot,
                    PendingQuery {
                        job: Some(QueryJob::submit(Arc::clone(&self.graph), plan.clone())),
                        result: None,
                        charged: 0,
                    },
                );
            }
            slot = plan.queue_next;
        }
    }
    fn publish(&mut self, now_tick: int, budget: int, wait: bool, drain: bool) -> bool {
        self.query.advance_spent = 0;
        self.submit();
        if nav_graph_refresh_pending(&self.graph) {
            return false;
        }
        while self.query.head >= 0 {
            let slot = self.query.head;
            if self.query.plans[slot].due_tick > now_tick {
                return false;
            }
            if !drain && self.query.advance_spent >= budget {
                return false;
            }
            let pending = self
                .jobs
                .get_mut(&slot)
                .expect("queued navigation job missing");
            if pending.result.is_none() {
                let job = pending
                    .job
                    .as_ref()
                    .expect("pending navigation query has no job");
                assert_eq!(
                    job.generation, self.query.plans[slot].generation,
                    "stale nav generation"
                );
                assert_eq!(job.epoch, self.graph.epoch, "stale nav graph epoch");
                let started = std::time::Instant::now();
                let result = job.take(wait);
                self.wait_ms += started.elapsed().as_secs_f64() * 1000.0;
                match result {
                    Some(Ok(result)) => {
                        pending.result = Some(result);
                        pending.job = None;
                    }
                    Some(Err(error)) => panic!("navigation query failed: {}", error),
                    None => return false,
                }
            }
            let result = pending.result.as_ref().unwrap();
            let remaining = result.work - pending.charged;
            let charge = if drain {
                remaining
            } else {
                min(remaining, budget - self.query.advance_spent)
            };
            pending.charged += charge;
            self.query.advance_spent += charge;
            if pending.charged < result.work {
                return false;
            }
            let pending = self.jobs.remove(&slot).unwrap();
            let result = pending.result.unwrap();
            assert!(
                result.plan.state == NAV_PLAN_READY || result.plan.state == NAV_PLAN_UNREACHABLE,
                "incomplete navigation worker result"
            );
            let mut old = std::mem::take(&mut self.query.plans[slot]);
            nav_unlink(&mut self.query, &mut old);
            let mut plan = result.plan;
            plan.queue_prev = -1;
            plan.queue_next = -1;
            self.query.plans[slot] = plan;
            self.worker_ms += result.worker_ms;
            self.area_work.searches += result.area_work.searches;
            self.area_work.derivations += result.area_work.derivations;
            self.area_work.repeated_attachments += result.area_work.repeated_attachments;
            self.completed += 1;
            self.query.cert_link_visits += result.cert_link_visits;
        }
        true
    }
}
impl Drop for NativeNav {
    fn drop(&mut self) {
        self.cancel_jobs();
        if self.completed > 0 {
            rock_log::emit_wall_clock(
                ::log::Level::Info,
                "NAV",
                format_args!(
                "queries={} worker_ms={:.3} worker_ms_per_query={:.3} publication_wait_ms={:.3}",
                self.completed,self.worker_ms,self.worker_ms/self.completed as f64,self.wait_ms),
            );
        }
    }
}
static NAVS: OnceLock<Mutex<BTreeMap<int, Arc<Mutex<NativeNav>>>>> = OnceLock::new();
static NEXT: AtomicI64 = AtomicI64::new(1);
fn navs() -> &'static Mutex<BTreeMap<int, Arc<Mutex<NativeNav>>>> {
    NAVS.get_or_init(|| Mutex::new(BTreeMap::new()))
}
fn with_nav(handle: int, f: impl FnOnce(&mut NativeNav) -> int) -> int {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let nav = navs()
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .expect("unknown navigation handle");
        let mut nav = nav.lock().unwrap();
        f(&mut nav)
    }));
    match result {
        Ok(value) => value,
        Err(_) => {
            ::log::error!(target: "NAV", "native navigation operation failed (handle {})", handle);
            -9007199254740991
        }
    }
}
unsafe fn input<'a, T>(ptr: *const T, len: int) -> &'a [T] {
    let len = usize::try_from(len).expect("negative navigation input length");
    if len == 0 {
        return &[];
    }
    assert!(!ptr.is_null(), "null navigation input");
    assert!(
        len <= isize::MAX as usize / std::mem::size_of::<T>(),
        "navigation input too large"
    );
    std::slice::from_raw_parts(ptr, len)
}
fn links(values: &[NativeNavIntWire]) -> IdxVec<NavLinkInput> {
    assert_eq!(
        values.len() % LINK_LANES,
        0,
        "navigation link record length"
    );
    values
        .as_chunks::<LINK_LANES>()
        .0
        .iter()
        .map(|v| NavLinkInput {
            from: place(v[0].value(), v[1].value(), v[2].value()),
            to: place(v[3].value(), v[4].value(), v[5].value()),
            cost: v[6].value(),
            requirement: v[7].value(),
        })
        .collect::<Vec<_>>()
        .into()
}

/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_nav_create(
    layout: *const NativeNavIntWire,
    layout_len: int,
    blocked: *const u8,
    corners: *const NativeNavIntWire,
    extra: *const NativeNavIntWire,
    tiles: int,
    links: *const NativeNavIntWire,
    link_len: int,
    cluster_size: int,
) -> int {
    let result = std::panic::catch_unwind(|| {
        let layout = input(layout, layout_len);
        assert_eq!(
            layout.len() % SPACE_LANES,
            0,
            "navigation space record length"
        );
        let spaces: Vec<_> = layout
            .as_chunks::<SPACE_LANES>()
            .0
            .iter()
            .map(|v| NavSpaceLayout {
                space: SpaceId {
                    value: v[0].value(),
                },
                origin: iv(v[1].value(), v[2].value()),
            })
            .collect();
        let blocked = input(blocked, tiles)
            .iter()
            .map(|v| *v != 0)
            .collect::<Vec<_>>()
            .into();
        let corners = input(corners, tiles)
            .iter()
            .map(|v| v.value())
            .collect::<Vec<_>>()
            .into();
        let extra = input(extra, tiles)
            .iter()
            .map(|v| v.value())
            .collect::<Vec<_>>()
            .into();
        let links = self::links(input(links, link_len));
        let graph =
            nav_graph_create_owned(&spaces, blocked, corners, extra, &links.0, cluster_size);
        let mut query = NavQuery::default();
        nav_query_follow_size(&graph, &mut query);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(id > 0 && id < (1 << 53), "navigation handle overflow");
        navs().lock().unwrap().insert(
            id,
            Arc::new(Mutex::new(NativeNav {
                graph: Arc::new(graph),
                query,
                jobs: BTreeMap::new(),
                worker_ms: 0.0,
                wait_ms: 0.0,
                completed: 0,
                area_work: NavWorkCounters::default(),
                descend: None,
            })),
        );
        id
    });
    match result {
        Ok(id) => id,
        Err(_) => {
            ::log::error!(target: "NAV", "native navigation graph creation failed");
            0
        }
    }
}
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_fork(handle: int) -> int {
    with_nav(handle, |source| {
        let fork = source.fork();
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(id > 0 && id < (1 << 53), "navigation handle overflow");
        navs()
            .lock()
            .unwrap()
            .insert(id, Arc::new(Mutex::new(fork)));
        id
    })
}

/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_free(handle: int) {
    navs().lock().unwrap().remove(&handle);
}

/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_nav_set_links(
    handle: int,
    data: *const NativeNavIntWire,
    len: int,
) -> int {
    with_nav(handle, |n| {
        n.cancel_jobs();
        nav_graph_set_links(Arc::make_mut(&mut n.graph), &links(input(data, len)).0);
        nav_query_follow_size(&n.graph, &mut n.query);
        1
    })
}
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_edit(
    handle: int,
    op: int,
    a: int,
    b: int,
    c: int,
    d: int,
    e: int,
    f: int,
) -> int {
    with_nav(handle, |n| match op {
        0 => {
            let done = nav_graph_refresh(Arc::make_mut(&mut n.graph), a);
            if done {
                n.submit();
            }
            done as int
        }
        1 => {
            n.cancel_jobs();
            nav_graph_set_tile(
                Arc::make_mut(&mut n.graph),
                SpaceId { value: a },
                iv(b, c),
                d != 0,
                e,
                f,
            );
            1
        }
        2 => {
            n.cancel_jobs();
            nav_graph_invalidate(
                Arc::make_mut(&mut n.graph),
                SpaceId { value: a },
                rect(b, c, d, e),
            );
            1
        }
        3 => n.publish(int::MAX, a, true, false) as int,
        4 => n.publish(a, NAV_UNBOUNDED_BUDGET, true, true) as int,
        5 => nav_world_step_open(&n.graph, a, b, c, d, e) as int,
        6 => nav_graph_world_tile(&n.graph, place(a, b, c)).x,
        7 => nav_graph_world_tile(&n.graph, place(a, b, c)).y,
        8 => {
            n.submit();
            1
        }
        _ => panic!("unknown navigation edit {}", op),
    })
}
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_get(handle: int, op: int, a: int, b: int, c: int) -> int {
    with_nav(handle, |n| {
        let g = &n.graph;
        match op {
            0 => nav_graph_refresh_pending(g) as int,
            1 => g.refresh_spent,
            2 => nav_graph_node_limit(g),
            3 => g.cluster_space.len(),
            4 => g.k,
            5 => g.label_size.len(),
            7 => nav_graph_holds(g, place(a, b, c)) as int,
            8 => nav_tile_blocked(g, a, b, c) as int,
            9 => g.corners[nav_tile_index(g, a, b, c)],
            10 => nav_tile_extra(g, a, b, c),
            11 => nav_node_live(g, a) as int,
            12 => nav_node_place(g, a).space.value,
            13 => nav_node_place(g, a).grid.x,
            14 => nav_node_place(g, a).grid.y,
            15 => nav_node_partner(g, a),
            16 => nav_node_partner_cost(g, a),
            17 => nav_node_link(g, a),
            18 => nav_node_is_pad(g, a) as int,
            19 => nav_node_cluster(g, a),
            20 => nav_node_slot(g, a),
            21 | 38 => g.component[a],
            22 => nav_cluster_slot_limit(g, a),
            23 => nav_cluster_node(g, a, b),
            24 => nav_cluster_pending(g, a) as int,
            25 => nav_intra_cost(g, a, b, c),
            26 => nav_tile_cluster(g, a, b, c),
            27 => g.epoch,
            28 => n.query.advance_spent,
            29 => g.space_id.len(),
            30 => g.space_id[a],
            31 => g.space_ox[a],
            32 => g.space_oy[a],
            33 => g.cluster_x[a],
            34 => g.cluster_y[a],
            35 => g.cluster_size,
            36 => g.node_x[a],
            37 => g.node_y[a],
            39 => g.id_space.get(&a).copied().unwrap_or(-1),
            40 => n.query.cert_link_visits,
            41 => n.query.plans.len(),
            43 => g.cost.iter().filter(|cost| **cost == -1).count() as int,
            44 => (g.cost.0.capacity() * std::mem::size_of::<int>()) as int,
            45 => {
                if n.query.head < 0 {
                    -1
                } else {
                    n.query.plans[n.query.head].due_tick
                }
            }
            _ => panic!("unknown navigation read {}", op),
        }
    })
}
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_request(
    handle: int,
    space: int,
    x: int,
    y: int,
    goal_space: int,
    gx: int,
    gy: int,
    gw: int,
    gh: int,
    teleports: int,
    seed: int,
    due_tick: int,
) -> int {
    with_nav(handle, |n| {
        assert!(due_tick >= 0, "negative navigation request deadline");
        let id = nav_request(
            &n.graph,
            &mut n.query,
            place(space, x, y),
            NavRegion {
                space: SpaceId { value: goal_space },
                rect: rect(gx, gy, gw, gh),
            },
            teleports,
            seed,
        );
        n.query.plans[id.slot].due_tick = due_tick;
        n.submit();
        id.slot
    })
}
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_plan(
    handle: int,
    slot: int,
    generation: int,
    op: int,
    a: int,
    b: int,
    c: int,
) -> int {
    with_nav(handle, |n| {
        let id = NavPlanId { slot, generation };
        if op == 0 {
            return n.query.plans[slot].generation;
        }
        if op == 1 {
            if let Some(pending) = n.jobs.remove(&slot) {
                if let Some(job) = &pending.job {
                    job.cancel();
                }
            }
            nav_plan_release(&mut n.query, id);
            return 1;
        }
        match op {
            2 => nav_plan_pending(&n.query, id) as int,
            3 => nav_plan_get(&n.query, id).expansions,
            4 => nav_plan_cost(&n.query, id),
            5 => nav_plan_teleport_link(&n.query, id),
            6 => nav_plan_link(&n.query, id, a),
            7 => nav_plan_remaining_cost(&n.graph, nav_plan_get(&n.query, id), place(a, b, c)),
            8 => nav_plan_get(&n.query, id).route.len(),
            9 => nav_plan_get(&n.query, id).path_pos,
            10 => nav_plan_get(&n.query, id).walked_cost,
            11 => nav_plan_get(&n.query, id).si,
            12 => nav_plan_get(&n.query, id).from_x,
            13 => nav_plan_get(&n.query, id).from_y,
            14 => nav_plan_get(&n.query, id).goal_si,
            15 => nav_plan_get(&n.query, id).goal_x,
            16 => nav_plan_get(&n.query, id).goal_y,
            17 => nav_plan_get(&n.query, id).state,
            18 => nav_plan_get(&n.query, id).due_tick,
            _ => panic!("unknown navigation plan read {}", op),
        }
    })
}
/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_nav_step(
    handle: int,
    slot: int,
    generation: int,
    space: int,
    x: int,
    y: int,
    due_tick: int,
    out: *mut NativeNavIntWire,
) -> int {
    with_nav(handle, |n| {
        assert!(due_tick >= 0, "negative navigation step deadline");
        nav_plan_get(&n.query, NavPlanId { slot, generation });
        let mut plan = std::mem::take(&mut n.query.plans[slot]);
        let was_pending = plan.state == NAV_PLAN_QUEUED;
        let step = nav_next_step(&n.graph, &mut n.query, &mut plan, place(space, x, y));
        if !was_pending && plan.state == NAV_PLAN_QUEUED {
            plan.due_tick = due_tick;
        }
        n.query.plans[slot] = plan;
        n.submit();
        match step {
            NavStep::Pending => 0,
            NavStep::Reached => 3,
            NavStep::Unreachable => 4,
            NavStep::Blocked => 5,
            NavStep::Step(p) | NavStep::Teleport(p) => {
                write_place(out, p);
                if matches!(step, NavStep::Step(_)) {
                    1
                } else {
                    2
                }
            }
        }
    })
}
/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_nav_route_copy(
    handle: int,
    slot: int,
    generation: int,
    out: *mut NativeNavIntWire,
    len: int,
) -> int {
    with_nav(handle, |n| {
        let plan = nav_plan_get(&n.query, NavPlanId { slot, generation });
        if len < plan.route.len() * ROUTE_LANES as int {
            return -1;
        }
        if plan.route.len() > 0 {
            assert!(!out.is_null(), "null navigation route output");
        }
        for (i, tile) in plan.route.iter().enumerate() {
            *out.add(i * ROUTE_LANES) = tile.x.into();
            *out.add(i * ROUTE_LANES + 1) = tile.y.into();
            *out.add(i * ROUTE_LANES + 2) = tile.link.into();
        }
        plan.route.len()
    })
}

/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_nav_route_restore(
    handle: int,
    slot: int,
    generation: int,
    data: *const NativeNavIntWire,
    len: int,
    path_pos: int,
    cost: int,
    walked: int,
) -> int {
    with_nav(handle, |n| {
        let values = input(data, len);
        assert_eq!(
            values.len() % ROUTE_LANES,
            0,
            "navigation route record length"
        );
        let id = NavPlanId { slot, generation };
        nav_plan_get(&n.query, id);
        let route_len = (values.len() / ROUTE_LANES) as int;
        assert!(route_len > 0, "cannot restore an empty navigation route");
        assert!(
            path_pos >= 0 && path_pos < route_len,
            "navigation route position is out of range"
        );
        let mut route = Vec::with_capacity(route_len as usize);
        for v in values.as_chunks::<ROUTE_LANES>().0 {
            let tile = NavRouteTile {
                x: v[0].value(),
                y: v[1].value(),
                link: v[2].value(),
            };
            assert!(
                tile.link >= 0 && tile.link <= n.graph.link_cost.len(),
                "navigation route link is out of range"
            );
            let si = nav_world_space(&n.graph, tile.x, tile.y, -1);
            assert!(si >= 0, "navigation route tile is outside every space");
            let (x, y) = (tile.x - n.graph.space_ox[si], tile.y - n.graph.space_oy[si]);
            route.push((tile, nav_tile_cluster(&n.graph, si, x, y)));
        }

        if let Some(pending) = n.jobs.remove(&slot) {
            if let Some(job) = &pending.job {
                job.cancel();
            }
        }
        let mut plan = std::mem::take(&mut n.query.plans[slot]);
        if plan.state == NAV_PLAN_QUEUED {
            nav_unlink(&mut n.query, &mut plan);
        }
        plan.route.clear();
        for &(tile, _) in &route {
            plan.route.push(tile);
        }
        plan.path_pos = path_pos;
        plan.state = NAV_PLAN_READY;
        plan.epoch = n.graph.epoch;
        plan.versions.clear();
        for &(_, cluster) in route.iter().rev() {
            nav_add_version(&n.graph, &mut plan, cluster);
        }
        nav_plan_restore_route_costs(
            &n.graph,
            &mut plan,
            if cost < 0 { None } else { Some(cost) },
            walked,
        );
        n.query.plans[slot] = plan;
        1
    })
}

/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_edge_cost(step: int, ea: int, eb: int) -> int {
    nav_edge_cost(step, ea, eb)
}

/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_metric(handle: int, field: int) -> f64 {
    let nav = navs().lock().unwrap().get(&handle).cloned();
    let Some(nav) = nav else {
        return -1.0;
    };
    let n = nav.lock().unwrap();
    match field {
        0 => n.completed as f64,
        // Profiling branches on these, so they would steer fixed-work runs by thread timing.
        1 | 2 if crate::image_decode::fixed_perf_work() => 0.0,
        1 => n.worker_ms,
        2 => n.wait_ms,
        3 => n.area_work.derivations as f64,
        4 => n.area_work.searches as f64,
        5 => n.area_work.repeated_attachments as f64,
        _ => -1.0,
    }
}

unsafe fn write_place(out: *mut NativeNavIntWire, p: Place) {
    assert!(!out.is_null(), "null navigation place output");
    let row: PlaceRow = [p.space.value.into(), p.grid.x.into(), p.grid.y.into()];
    out.cast::<PlaceRow>().write(row);
}
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_descend_prepare(
    handle: int,
    slot: int,
    generation: int,
    space: int,
    x: int,
    y: int,
) -> int {
    with_nav(handle, |n| {
        nav_plan_get(&n.query, NavPlanId { slot, generation });
        let plan = std::mem::take(&mut n.query.plans[slot]);
        n.descend = nav_descend_prepare(&n.graph, &mut n.query, &plan, place(space, x, y));
        n.query.plans[slot] = plan;
        n.descend.as_ref().map_or(0, |s| s.candidates.len() as int)
    })
}
/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_nav_descend_candidate(
    handle: int,
    index: int,
    phase: int,
    out: *mut NativeNavIntWire,
) -> int {
    with_nav(handle, |n| {
        if let Some(p) = n
            .descend
            .as_ref()
            .and_then(|s| s.candidate(index as usize, phase))
        {
            write_place(out, p);
            1
        } else {
            0
        }
    })
}
/// rock-ffi stone-native
#[no_mangle]
pub extern "C" fn stone_nav_descend_accept(handle: int, index: int, phase: int) -> int {
    with_nav(handle, |n| {
        n.descend
            .as_mut()
            .expect("missing nav descent")
            .accept(index as usize, phase);
        1
    })
}
/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_nav_descend_result(handle: int, out: *mut NativeNavIntWire) -> int {
    with_nav(handle, |n| {
        if let Some(p) = n.descend.take().and_then(|s| s.chosen) {
            write_place(out, p);
            1
        } else {
            0
        }
    })
}

/// rock-ffi stone-native
#[no_mangle]
pub unsafe extern "C" fn stone_nav_layout_valid(
    data: *const NativeNavIntWire,
    len: int,
    cluster: int,
) -> int {
    std::panic::catch_unwind(|| {
        let values = input(data, len);
        if values.len() % SPACE_LANES != 0 {
            return 0;
        }
        let spaces: Vec<_> = values
            .as_chunks::<SPACE_LANES>()
            .0
            .iter()
            .map(|v| NavSpaceLayout {
                space: SpaceId {
                    value: v[0].value(),
                },
                origin: iv(v[1].value(), v[2].value()),
            })
            .collect();
        nav_graph_layout_input_error(&spaces, cluster).is_none() as int
    })
    .unwrap_or_else(|_| {
        ::log::error!(target:"NAV","invalid native navigation layout input");
        0
    })
}

pub(super) fn shutdown() {
    navs().lock().unwrap().clear();
}

#[cfg(test)]
pub(super) struct Owned(pub(super) int);

#[cfg(test)]
impl Drop for Owned {
    fn drop(&mut self) {
        stone_nav_free(self.0);
    }
}

#[cfg(test)]
pub(super) fn create_open_graph() -> Owned {
    let layout = [1.into(), 0.into(), 0.into()];
    let count = (SPACE_SIZE * SPACE_SIZE) as usize;
    let blocked = vec![0u8; count];
    let values: Vec<NativeNavIntWire> = (0..count).map(|_| 0.into()).collect();
    let graph = Owned(unsafe {
        stone_nav_create(
            layout.as_ptr(),
            layout.len() as int,
            blocked.as_ptr(),
            values.as_ptr(),
            values.as_ptr(),
            count as int,
            std::ptr::null(),
            0,
            8,
        )
    });
    assert!(graph.0 > 0, "native graph creation succeeds");
    graph
}

#[cfg(test)]
mod fork_tests {
    use super::*;

    #[test]
    fn fixed_perf_work_hides_native_navigation_durations() {
        use crate::test_support::{assert_child_passes, child_test};
        const TEST: &str =
            "nav::ffi::fork_tests::fixed_perf_work_hides_native_navigation_durations";
        const CHILD: &str = "STONE_NAV_FIXED_WORK_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            assert_child_passes(
                child_test(TEST, CHILD, "1")
                    .env("PERF_MODE", "1")
                    .env("PERF_FIXED_WORK", "1"),
                TEST,
            );
            return;
        }
        let nav = create_open_graph();
        with_nav(nav.0, |nav| {
            nav.completed = 3;
            nav.worker_ms = 1.5;
            nav.wait_ms = 2.5;
            1
        });
        let metrics: Vec<f64> = (0..3).map(|field| stone_nav_metric(nav.0, field)).collect();
        assert_eq!(metrics, [3.0, 0.0, 0.0]);
    }

    #[test]
    fn fork_preserves_pending_budget_and_isolates_edits_and_retirement() {
        let parent = create_open_graph();
        assert_eq!(stone_nav_edit(parent.0, 0, 1, 0, 0, 0, 0, 0), 0);
        let rebuilding = Owned(stone_nav_fork(parent.0));
        assert_ne!(rebuilding.0, parent.0);
        assert_eq!(
            stone_nav_edit(rebuilding.0, 0, NAV_UNBOUNDED_BUDGET, 0, 0, 0, 0, 0),
            1
        );
        with_nav(parent.0, |nav| {
            assert!(nav_graph_refresh_pending(&nav.graph));
            1
        });
        assert_eq!(
            stone_nav_edit(parent.0, 0, NAV_UNBOUNDED_BUDGET, 0, 0, 0, 0, 0),
            1
        );
        let slot = stone_nav_request(parent.0, 1, 1, 1, 1, 90, 90, 1, 1, 0, 42, 12);
        let generation = stone_nav_plan(parent.0, slot, -1, 0, 0, 0, 0);
        let queued = Owned(stone_nav_fork(parent.0));
        assert_eq!(stone_nav_get(queued.0, 45, 0, 0, 0), 12);
        assert_eq!(stone_nav_edit(queued.0, 4, 11, 0, 0, 0, 0, 0), 0);
        drop(queued);
        with_nav(parent.0, |nav| {
            assert!(!nav.publish(12, 1, true, false));
            assert_eq!(nav.jobs[&slot].charged, 1);
            1
        });
        let branch = Owned(stone_nav_fork(parent.0));
        let mut ready = false;
        for _ in 0..100000 {
            let a = with_nav(parent.0, |nav| nav.publish(12, 1, true, false) as int);
            let b = with_nav(branch.0, |nav| nav.publish(12, 1, true, false) as int);
            assert_eq!(
                a, b,
                "fork retains already charged work and publication order"
            );
            if a == 1 {
                ready = true;
                break;
            }
        }
        assert!(ready);
        assert_eq!(stone_nav_plan(branch.0, slot, -1, 0, 0, 0, 0), generation);
        let source_route = with_nav(parent.0, |nav| nav.query.plans[slot].route.len());
        assert!(source_route > 0);
        assert_eq!(
            with_nav(branch.0, |nav| nav.query.plans[slot].route.len()),
            source_route
        );
        assert_eq!(stone_nav_edit(branch.0, 1, 1, 4, 4, 1, 0, 0), 1);
        with_nav(parent.0, |nav| {
            assert!(!nav_tile_blocked(&nav.graph, 0, 4, 4));
            assert_eq!(nav.query.plans[slot].route.len(), source_route);
            1
        });
        drop(parent);
        assert_eq!(
            stone_nav_edit(branch.0, 0, NAV_UNBOUNDED_BUDGET, 0, 0, 0, 0, 0),
            1
        );
    }
}
