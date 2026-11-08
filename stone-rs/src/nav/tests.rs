use super::*;

#[test]
fn routing_entropy_matches_luajit_number_conversion() {
    // Captured from the original Rock route_hash on arm64 LuaJIT 2.1.
    // Large products exercise its numeric ToBit conversion, not integer modulo.
    for (x, expected) in [
        (1, 1433332313),
        (1000000, 624030942),
        (1000000000, 1096535438),
        (-1000000000, 1960476970),
    ] {
        assert_eq!(route_hash(42, x, 3), expected);
    }
}

#[derive(Default)]
struct NavTestSpace {
    space: SpaceId,
    origin: IVec2,
    blocked: IdxVec<bool>,
}
fn nav_test_graph(spaces: &[NavTestSpace], links: &[NavLinkInput], cluster: int) -> NavGraph {
    let layouts: Vec<_> = spaces
        .iter()
        .map(|s| NavSpaceLayout {
            space: s.space,
            origin: s.origin,
        })
        .collect();
    let blocked: Vec<_> = spaces
        .iter()
        .flat_map(|s| s.blocked.0.iter().copied())
        .collect();
    let count = blocked.len() as int;
    let mut graph = nav_graph_create_owned(
        &layouts,
        blocked.into(),
        nav_ints(count, 0),
        nav_ints(count, 0),
        links,
        cluster,
    );
    assert!(nav_graph_refresh(&mut graph, NAV_UNBOUNDED_BUDGET));
    graph
}

fn nav_shortcut_bellman_ford(input: &NavTestSpace, side: int, from: IVec2, goal: IVec2) -> int {
    let index = |x: int, y: int| (y * side + x) as usize;
    let mut distance = vec![int::MAX / 4; (side * side) as usize];
    distance[index(from.x, from.y)] = 0;

    for _ in 1..(side * side) {
        let mut changed = false;
        for y in 0..side {
            for x in 0..side {
                let cost = distance[index(x, y)];
                if cost == int::MAX / 4 {
                    continue;
                }
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let nx = x + dx;
                        let ny = y + dy;
                        if nx < 0
                            || ny < 0
                            || nx >= side
                            || ny >= side
                            || input.blocked[ny * SPACE_SIZE + nx]
                        {
                            continue;
                        }
                        let hop = if dx != 0 && dy != 0 {
                            NAV_COST_DIAGONAL
                        } else {
                            NAV_COST_STRAIGHT
                        };
                        let next = index(nx, ny);
                        if cost + hop < distance[next] {
                            distance[next] = cost + hop;
                            changed = true;
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    distance[index(goal.x, goal.y)]
}

#[test]
fn nav_shortcut_erases_a_loop_longer_than_its_neighbor_window() {
    let side = 12;
    let mut blocked = vec![true; (SPACE_SIZE * SPACE_SIZE) as usize];
    for y in 0..side {
        for x in 0..side {
            blocked[(y * SPACE_SIZE + x) as usize] = false;
        }
    }
    let input = NavTestSpace {
        space: SpaceId { value: 1 },
        origin: iv(0, 0),
        blocked: IdxVec::from(blocked),
    };
    let graph = nav_test_graph(std::slice::from_ref(&input), &[], 8);
    let mut plan = NavPlan {
        si: 0,
        ..NavPlan::default()
    };
    for (x, y) in [
        (7, 1),
        (7, 2),
        (7, 3),
        (6, 4),
        (5, 5),
        (4, 6),
        (3, 7),
        (3, 8),
        (3, 7),
        (4, 6),
        (5, 5),
        (6, 4),
        (6, 3),
        (7, 2),
        (8, 2),
        (8, 3),
    ] {
        nav_path_push(&graph, &mut plan, x, y, 0);
    }
    nav_route_shortcut(&graph, &mut plan);

    let mut seen = std::collections::BTreeSet::new();
    let mut walked_cost = 0;
    for i in 0..plan.route.len() {
        let tile = plan.route[i];
        assert!(
            seen.insert((tile.x, tile.y)),
            "the published shortcut revisits {},{}",
            tile.x,
            tile.y
        );
        if i > 0 {
            let prev = plan.route[i - 1];
            let dx = tile.x - prev.x;
            let dy = tile.y - prev.y;
            assert!(
                dx.abs() <= 1
                    && dy.abs() <= 1
                    && (dx != 0 || dy != 0)
                    && nav_world_step_open(&graph, prev.x, prev.y, tile.x, tile.y, plan.si),
                "shortcut steps stay legal"
            );
            walked_cost += if dx != 0 && dy != 0 {
                NAV_COST_DIAGONAL
            } else {
                NAV_COST_STRAIGHT
            };
        }
    }
    assert!(
        walked_cost >= nav_shortcut_bellman_ford(&input, side, iv(7, 1), iv(8, 3)),
        "the shortcut retains a walk at least as costly as the independent tile optimum"
    );
}

#[test]
fn native_query_results_are_off_thread_and_wait_for_publication() {
    use std::sync::Arc;
    let blocked = vec![false; (SPACE_SIZE * SPACE_SIZE) as usize];
    let input = NavTestSpace {
        space: SpaceId { value: 1 },
        blocked: blocked.into(),
        ..NavTestSpace::default()
    };
    let graph = Arc::new(nav_test_graph(&[input], &[], 8));
    let mut query = NavQuery::default();
    nav_query_follow_size(&graph, &mut query);
    let from = Place {
        space: SpaceId { value: 1 },
        grid: iv(1, 1),
    };
    let goal = NavRegion {
        space: SpaceId { value: 1 },
        rect: IRect {
            x: 30,
            y: 30,
            w: 1,
            h: 1,
        },
    };
    let id = nav_request(&graph, &mut query, from, goal, 0, 42);
    let job = QueryJob::submit(Arc::clone(&graph), query.plans[id.slot].clone());
    let result = job.take(true).unwrap().unwrap();
    assert_ne!(result.worker_thread, std::thread::current().id());
    assert_eq!(result.plan.state, NAV_PLAN_READY);
    assert!(
        nav_plan_pending(&query, id),
        "worker completion cannot publish its plan slot"
    );
    assert!(
        query.plans[id.slot].route.len() == 0,
        "unpublished route storage is unchanged"
    );
    nav_plan_release(&mut query, id);
    let replacement = nav_request(&graph, &mut query, from, goal, 0, 43);
    assert_eq!(replacement.slot, id.slot);
    assert_ne!(
        replacement.generation, result.plan.generation,
        "cancelled results cannot bind a reused slot"
    );
    job.wait_finished();
}

#[test]
fn native_publication_waits_for_each_queued_deadline_in_fifo_order() {
    use super::ffi::{
        stone_nav_create, stone_nav_edit, stone_nav_get, stone_nav_plan, stone_nav_request,
        stone_nav_route_restore, stone_nav_step, NativeNavIntWire,
    };

    struct Handle(i64);
    impl Drop for Handle {
        fn drop(&mut self) {
            super::ffi::stone_nav_free(self.0);
        }
    }

    let layout = [1.into(), 0.into(), 0.into()];
    let tile_count = (SPACE_SIZE * SPACE_SIZE) as usize;
    let blocked = vec![0u8; tile_count];
    let values: Vec<NativeNavIntWire> = (0..tile_count).map(|_| 0.into()).collect();
    let handle = unsafe {
        stone_nav_create(
            layout.as_ptr(),
            layout.len() as int,
            blocked.as_ptr(),
            values.as_ptr(),
            values.as_ptr(),
            tile_count as int,
            std::ptr::null(),
            0,
            8,
        )
    };
    assert!(handle > 0, "native graph creation succeeds");
    let _handle = Handle(handle);
    assert_eq!(
        stone_nav_edit(handle, 0, NAV_UNBOUNDED_BUDGET, 0, 0, 0, 0, 0),
        1
    );

    let first = stone_nav_request(handle, 1, 1, 1, 1, 30, 30, 1, 1, 0, 42, 12);
    let second = stone_nav_request(handle, 1, 1, 1, 1, 30, 30, 1, 1, 0, 43, 13);
    assert_ne!(first, second, "queued requests have distinct plan slots");
    let first_generation = stone_nav_plan(handle, first, -1, 0, 0, 0, 0);
    let second_generation = stone_nav_plan(handle, second, -1, 0, 0, 0, 0);
    let plan = |slot, generation, op| stone_nav_plan(handle, slot, generation, op, 0, 0, 0);

    assert_eq!(plan(first, first_generation, 2), 1);
    assert_eq!(plan(second, second_generation, 2), 1);
    assert_eq!(plan(first, first_generation, 18), 12);
    assert_eq!(plan(second, second_generation, 18), 13);
    assert_eq!(stone_nav_get(handle, 45, 0, 0, 0), 12);

    assert_eq!(stone_nav_edit(handle, 4, 11, 0, 0, 0, 0, 0), 0);
    assert_eq!(plan(first, first_generation, 2), 1);
    assert_eq!(plan(second, second_generation, 2), 1);

    assert_eq!(stone_nav_edit(handle, 4, 12, 0, 0, 0, 0, 0), 0);
    assert_eq!(plan(first, first_generation, 2), 0);
    assert_eq!(plan(second, second_generation, 2), 1);
    assert_eq!(stone_nav_get(handle, 45, 0, 0, 0), 13);
    assert_eq!(
        stone_nav_plan(handle, first, -1, 0, 0, 0, 0),
        first_generation,
        "publication retains the request's slot generation"
    );

    assert_eq!(stone_nav_edit(handle, 4, 13, 0, 0, 0, 0, 0), 1);
    assert_eq!(plan(second, second_generation, 2), 0);
    assert_eq!(stone_nav_get(handle, 45, 0, 0, 0), -1);
    assert_eq!(
        stone_nav_plan(handle, second, -1, 0, 0, 0, 0),
        second_generation,
        "the second worker result publishes to its original generation"
    );

    let future = stone_nav_request(handle, 1, 1, 1, 1, 30, 30, 1, 1, 0, 44, 20);
    let restored = stone_nav_request(handle, 1, 1, 1, 1, 3, 1, 1, 1, 0, 45, 21);
    let future_generation = stone_nav_plan(handle, future, -1, 0, 0, 0, 0);
    let restored_generation = stone_nav_plan(handle, restored, -1, 0, 0, 0, 0);
    let saved_route_values: [int; 9] = [1, 1, 0, 2, 1, 0, 3, 1, 0];
    let saved_route: Vec<NativeNavIntWire> = saved_route_values
        .into_iter()
        .map(NativeNavIntWire::from)
        .collect();
    assert_eq!(
        unsafe {
            stone_nav_route_restore(
                handle,
                restored,
                restored_generation,
                saved_route.as_ptr(),
                saved_route.len() as int,
                0,
                -1,
                0,
            )
        },
        1
    );
    assert_eq!(
        plan(future, future_generation, 2),
        1,
        "restoring a later route leaves the earlier future request pending"
    );
    assert_eq!(plan(restored, restored_generation, 2), 0);
    assert_eq!(stone_nav_get(handle, 45, 0, 0, 0), 20);
    assert_eq!(plan(restored, restored_generation, 8), 3);
    assert_eq!(plan(restored, restored_generation, 4), 2000);

    let mut output = [NativeNavIntWire::from(0); 3];
    assert_eq!(
        unsafe {
            stone_nav_step(
                handle,
                restored,
                restored_generation,
                1,
                1,
                1,
                0,
                output.as_mut_ptr(),
            )
        },
        1,
        "the restored route supplies a native next step"
    );
    assert_eq!(
        unsafe {
            stone_nav_step(
                handle,
                restored,
                restored_generation,
                1,
                2,
                1,
                0,
                output.as_mut_ptr(),
            )
        },
        1
    );
    assert_eq!(plan(restored, restored_generation, 9), 1);
    assert_eq!(plan(restored, restored_generation, 10), 1000);
    assert_eq!(plan(future, future_generation, 2), 1);
    assert_eq!(stone_nav_edit(handle, 4, 19, 0, 0, 0, 0, 0), 0);
    assert_eq!(plan(future, future_generation, 2), 1);
    assert_eq!(plan(restored, restored_generation, 2), 0);
    assert_eq!(
        stone_nav_plan(handle, restored, -1, 0, 0, 0, 0),
        restored_generation,
        "restored routes retain their slot generation"
    );
}
