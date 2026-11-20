//! Drives random graphs, edits and queries through the C exports and hashes
//! every observable answer, so a change to the navigation code that alters a
//! path, cost, charged budget, node numbering or component label fails here.
use super::ffi::*;
use super::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> int {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 1) as int
    }
    fn below(&mut self, n: int) -> int {
        self.next() % n
    }
    fn chance(&mut self, percent: int) -> bool {
        self.below(100) < percent
    }
}

struct Hasher(u64, u64);
impl Hasher {
    fn put(&mut self, value: int) {
        self.0 = (self.0 ^ value as u64).wrapping_mul(0x100000001b3);
        self.1 += 1;
    }
}

struct World {
    handle: Owned,
    origins: Vec<(int, int, int)>,
    tiles: Vec<bool>,
}
impl World {
    fn place_of(&self, wx: int, wy: int) -> Option<(int, int, int)> {
        self.origins.iter().find_map(|&(id, ox, oy)| {
            let (x, y) = (wx - ox, wy - oy);
            (x >= 0 && y >= 0 && x < SPACE_SIZE && y < SPACE_SIZE).then_some((id, x, y))
        })
    }
    fn open_place(&self, rng: &mut Rng) -> (int, int, int) {
        for _ in 0..64 {
            let si = rng.below(self.origins.len() as int);
            let (x, y) = (rng.below(SPACE_SIZE), rng.below(SPACE_SIZE));
            if !self.tiles[(si * SPACE_SIZE * SPACE_SIZE + y * SPACE_SIZE + x) as usize] {
                return (self.origins[si as usize].0, x, y);
            }
        }
        (self.origins[0].0, 0, 0)
    }
}

fn wire(values: &[int]) -> Vec<NativeNavIntWire> {
    values.iter().map(|&v| v.into()).collect()
}

fn random_links(rng: &mut Rng, world: &World, count: int) -> Vec<NativeNavIntWire> {
    let mut values = Vec::new();
    for _ in 0..count {
        let (a, ax, ay) = world.open_place(rng);
        let (b, bx, by) = world.open_place(rng);
        let cost = rng.below(20000);
        let bit = 1 << rng.below(3);
        values.extend_from_slice(&[a, ax, ay, b, bx, by, cost, bit]);
    }
    wire(&values)
}

fn build(rng: &mut Rng, cluster: int) -> World {
    let layouts: &[&[(int, int)]] = &[
        &[(0, 0)],
        &[(0, 0), (120, 0)],
        &[(0, 0), (0, 120), (120, 120)],
        &[(-120, 0), (480, 240)],
    ];
    let layout = layouts[rng.below(layouts.len() as int) as usize];
    let origins: Vec<_> = layout
        .iter()
        .enumerate()
        .map(|(i, &(x, y))| (10 + 3 * i as int, x, y))
        .collect();
    let count = (origins.len() as int * SPACE_SIZE * SPACE_SIZE) as usize;
    let density = 5 + rng.below(30);
    let mut tiles: Vec<bool> = (0..count).map(|_| rng.chance(density)).collect();
    for _ in 0..rng.below(12) * origins.len() as int {
        let base = rng.below(origins.len() as int) * SPACE_SIZE * SPACE_SIZE;
        let (x, y) = (rng.below(SPACE_SIZE), rng.below(SPACE_SIZE));
        let vertical = rng.chance(50);
        let gap = rng.below(SPACE_SIZE);
        for i in 0..rng.below(90) {
            let (tx, ty) = if vertical { (x, y + i) } else { (x + i, y) };
            if tx < SPACE_SIZE && ty < SPACE_SIZE && (i - gap).abs() > 1 {
                tiles[(base + ty * SPACE_SIZE + tx) as usize] = true;
            }
        }
    }
    let corners: Vec<int> = tiles
        .iter()
        .map(|&b| if b { rng.below(16) } else { 0 })
        .collect();
    let mut extra = vec![0; count];
    if rng.chance(60) {
        for _ in 0..rng.below(20) {
            let base = rng.below(origins.len() as int) * SPACE_SIZE * SPACE_SIZE;
            let (x, y, w, h) = (
                rng.below(SPACE_SIZE),
                rng.below(SPACE_SIZE),
                rng.below(30),
                rng.below(30),
            );
            let value = rng.below(400);
            for ty in y..(y + h).min(SPACE_SIZE) {
                for tx in x..(x + w).min(SPACE_SIZE) {
                    extra[(base + ty * SPACE_SIZE + tx) as usize] = value;
                }
            }
        }
    }
    let flat: Vec<int> = origins.iter().flat_map(|&(id, x, y)| [id, x, y]).collect();
    let layout_wire = wire(&flat);
    let blocked: Vec<u8> = tiles.iter().map(|&b| b as u8).collect();
    let corners = wire(&corners);
    let extra = wire(&extra);
    let mut world = World {
        handle: Owned(0),
        origins,
        tiles,
    };
    let link_count = rng.below(6);
    let links = random_links(rng, &world, link_count);
    world.handle = Owned(unsafe {
        stone_nav_create(
            layout_wire.as_ptr(),
            layout_wire.len() as int,
            blocked.as_ptr(),
            corners.as_ptr(),
            extra.as_ptr(),
            count as int,
            links.as_ptr(),
            links.len() as int,
            cluster,
        )
    });
    assert!(world.handle.0 > 0);
    world
}

fn refresh(rng: &mut Rng, h: &mut Hasher, handle: int) {
    loop {
        let budget = if rng.chance(20) {
            NAV_UNBOUNDED_BUDGET
        } else {
            1 + rng.below(400)
        };
        let done = stone_nav_edit(handle, 0, budget, 0, 0, 0, 0, 0);
        h.put(done);
        h.put(stone_nav_get(handle, 1, 0, 0, 0));
        if done == 1 {
            return;
        }
    }
}

fn dump_graph(h: &mut Hasher, handle: int) {
    let get = |op, a, b, c| stone_nav_get(handle, op, a, b, c);
    let nodes = get(2, 0, 0, 0);
    let clusters = get(3, 0, 0, 0);
    for v in [
        nodes,
        clusters,
        get(4, 0, 0, 0),
        get(5, 0, 0, 0),
        get(27, 0, 0, 0),
        get(43, 0, 0, 0),
    ] {
        h.put(v);
    }
    for n in 0..nodes {
        let link = get(17, n, 0, 0);
        h.put(get(11, n, 0, 0));
        h.put(get(36, n, 0, 0));
        h.put(get(37, n, 0, 0));
        h.put(get(15, n, 0, 0));
        if link < 0 {
            h.put(get(16, n, 0, 0));
        }
        h.put(link);
        h.put(get(18, n, 0, 0));
        h.put(get(19, n, 0, 0));
        h.put(get(20, n, 0, 0));
        h.put(get(21, n, 0, 0));
    }
    for c in 0..clusters {
        let slots = get(22, c, 0, 0);
        h.put(slots);
        h.put(get(24, c, 0, 0));
        for i in 0..slots {
            h.put(get(23, c, i, 0));
            for j in 0..slots {
                h.put(get(25, c, i, j));
            }
        }
    }
}

fn plan_summary(h: &mut Hasher, handle: int, slot: int, generation: int) -> Vec<int> {
    let plan = |op, a| stone_nav_plan(handle, slot, generation, op, a, 0, 0);
    for op in [2, 3, 4, 5, 8, 9, 10, 17] {
        h.put(plan(op, 0));
    }
    for i in 0..3 {
        h.put(plan(6, i));
    }
    let len = plan(8, 0);
    let mut out = vec![NativeNavIntWire::from(0); (3 * len) as usize];
    let copied =
        unsafe { stone_nav_route_copy(handle, slot, generation, out.as_mut_ptr(), 3 * len) };
    h.put(copied);
    let route: Vec<int> = out.iter().map(|w| w.value()).collect();
    for &v in &route {
        h.put(v);
    }
    route
}

fn publish(rng: &mut Rng, h: &mut Hasher, handle: int) {
    for _ in 0..100000 {
        if stone_nav_get(handle, 0, 0, 0, 0) == 1 {
            refresh(rng, h, handle);
        }
        let done = if rng.chance(10) {
            stone_nav_edit(handle, 4, rng.below(40), 0, 0, 0, 0, 0)
        } else {
            stone_nav_edit(handle, 3, 1 + rng.below(3000), 0, 0, 0, 0, 0)
        };
        h.put(done);
        h.put(stone_nav_get(handle, 28, 0, 0, 0));
        h.put(stone_nav_get(handle, 45, 0, 0, 0));
        if done == 1 {
            return;
        }
    }
    panic!("navigation publication never finished");
}

fn request(rng: &mut Rng, world: &World) -> [int; 11] {
    let (s, x, y) = world.open_place(rng);
    let (gs, gx, gy) = world.open_place(rng);
    let (gw, gh) = if rng.chance(70) {
        (1, 1)
    } else {
        (1 + rng.below(12), 1 + rng.below(12))
    };
    let teleports = if rng.chance(50) { rng.below(8) } else { 0 };
    [
        s,
        x,
        y,
        gs,
        gx - gw / 2,
        gy - gh / 2,
        gw,
        gh,
        teleports,
        rng.below(1 << 30),
        rng.below(30),
    ]
}

fn walk(rng: &mut Rng, h: &mut Hasher, world: &World, slot: int, generation: int, steps: int) {
    let handle = world.handle.0;
    let mut at = (
        stone_nav_get(
            handle,
            30,
            stone_nav_plan(handle, slot, generation, 11, 0, 0, 0),
            0,
            0,
        ),
        stone_nav_plan(handle, slot, generation, 12, 0, 0, 0),
        stone_nav_plan(handle, slot, generation, 13, 0, 0, 0),
    );
    for _ in 0..steps {
        if rng.chance(8) {
            let (dx, dy) = (rng.below(3) - 1, rng.below(3) - 1);
            let (nx, ny) = (at.1 + dx, at.2 + dy);
            if nx >= 0 && ny >= 0 && nx < SPACE_SIZE && ny < SPACE_SIZE {
                at = (at.0, nx, ny);
            }
        }
        if rng.chance(15) {
            let count = stone_nav_descend_prepare(handle, slot, generation, at.0, at.1, at.2);
            h.put(count);
            let mut out = [NativeNavIntWire::from(0); 3];
            for phase in 0..2 {
                for i in 0..count {
                    let ok =
                        unsafe { stone_nav_descend_candidate(handle, i, phase, out.as_mut_ptr()) };
                    h.put(ok);
                    if ok == 1 && rng.chance(70) {
                        stone_nav_descend_accept(handle, i, phase);
                    }
                }
            }
            h.put(unsafe { stone_nav_descend_result(handle, out.as_mut_ptr()) });
            for v in out {
                h.put(v.value());
            }
        }
        h.put(stone_nav_plan(
            handle, slot, generation, 7, at.0, at.1, at.2,
        ));
        let mut out = [NativeNavIntWire::from(0); 3];
        let step = unsafe {
            stone_nav_step(
                handle,
                slot,
                generation,
                at.0,
                at.1,
                at.2,
                rng.below(5),
                out.as_mut_ptr(),
            )
        };
        h.put(step);
        match step {
            1 => {
                let v = out.map(|w| w.value());
                for x in v {
                    h.put(x);
                }
                at = (v[0], v[1], v[2]);
            }
            2 => {
                let route = plan_summary(h, handle, slot, generation);
                let pos = stone_nav_plan(handle, slot, generation, 9, 0, 0, 0) as usize;
                let next = &route[3 * (pos + 1)..3 * (pos + 2)];
                at = world.place_of(next[0], next[1]).unwrap();
            }
            0 => publish(rng, h, handle),
            _ => return,
        }
    }
}

fn scenario(seed: u64, h: &mut Hasher) {
    let mut rng = Rng(seed);
    let cluster = [4, 6, 8, 8, 10, 12][rng.below(6) as usize];
    let mut world = build(&mut rng, cluster);
    let handle = world.handle.0;
    refresh(&mut rng, h, handle);
    dump_graph(h, handle);
    for round in 0..3 {
        let mut plans = Vec::new();
        for _ in 0..1 + rng.below(6) {
            let r = request(&mut rng, &world);
            let slot = stone_nav_request(
                handle, r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8], r[9], r[10],
            );
            h.put(slot);
            plans.push((slot, stone_nav_plan(handle, slot, -1, 0, 0, 0, 0)));
        }
        publish(&mut rng, h, handle);
        for &(slot, generation) in &plans {
            plan_summary(h, handle, slot, generation);
        }
        for &(slot, generation) in &plans {
            if rng.chance(70) {
                walk(&mut rng, h, &world, slot, generation, 60);
            }
        }
        for _ in 0..rng.below(12) {
            let si = rng.below(world.origins.len() as int);
            let (id, x, y) = (
                world.origins[si as usize].0,
                rng.below(SPACE_SIZE),
                rng.below(SPACE_SIZE),
            );
            let t = (si * SPACE_SIZE * SPACE_SIZE + y * SPACE_SIZE + x) as usize;
            world.tiles[t] = !world.tiles[t];
            let blocked = world.tiles[t] as int;
            h.put(stone_nav_edit(
                handle,
                1,
                id,
                x,
                y,
                blocked,
                rng.below(16),
                rng.below(3) * 50,
            ));
            h.put(stone_nav_edit(handle, 2, id, x - 1, y - 1, 3, 3, 0));
        }
        if round == 1 && rng.chance(50) {
            let count = rng.below(5);
            let links = random_links(&mut rng, &world, count);
            h.put(unsafe { stone_nav_set_links(handle, links.as_ptr(), links.len() as int) });
        }
        for &(slot, generation) in &plans {
            if rng.chance(50) {
                walk(&mut rng, h, &world, slot, generation, 20);
            }
        }
        refresh(&mut rng, h, handle);
        if round == 1 {
            dump_graph(h, handle);
        }
        for &(slot, generation) in &plans {
            if rng.chance(60) {
                walk(&mut rng, h, &world, slot, generation, 40);
            }
            if rng.chance(30) {
                stone_nav_plan(handle, slot, generation, 1, 0, 0, 0);
            }
        }
        h.put(stone_nav_get(handle, 40, 0, 0, 0));
        h.put(stone_nav_metric(handle, 3) as int);
        h.put(stone_nav_metric(handle, 4) as int);
        h.put(stone_nav_metric(handle, 5) as int);
    }
}

#[test]
fn random_graphs_queries_and_edits_keep_their_answers() {
    let mut h = Hasher(0xcbf29ce484222325, 0);
    for seed in 1..=24u64 {
        scenario(seed.wrapping_mul(0x9e3779b97f4a7c15), &mut h);
    }
    assert_eq!((h.0, h.1), (0x2a5dce637398549e, 31501245));
}
