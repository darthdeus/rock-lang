use super::*;

pub const NAV_CLUSTER_SIZE_DEFAULT: int = 8;

pub const NAV_COST_STRAIGHT: int = 1000;

pub const NAV_COST_DIAGONAL: int = 1414;

pub const NAV_LONG_SPAN: int = 6;

pub const NAV_UNBOUNDED_BUDGET: int = 2000000000;

pub const NAV_PHASE_IDLE: int = 0;

pub const NAV_PHASE_BORDERS: int = 1;

pub const NAV_PHASE_SEARCH: int = 2;

pub const NAV_PHASE_COMPONENTS: int = 3;

pub const NAV_PHASE_VERTICES: int = 4;

pub const NAV_PHASE_BEGIN: int = 5;

pub const NAV_COMP_DETACH: int = 0;

pub const NAV_COMP_SEEDS: int = 1;

pub const NAV_COMP_RACE: int = 2;

pub const NAV_COMP_KEEP: int = 3;

pub const NAV_COMP_LABEL: int = 4;

pub const NAV_COMP_MERGE: int = 5;

pub const NAV_COMP_END: int = 6;

pub const NAV_STAMP_LIMIT: int = 2000000000;

pub const NAV_INF: int = 2000000000;

pub const NAV_PLAN_FREE: int = 0;

pub const NAV_PLAN_QUEUED: int = 1;

pub const NAV_PLAN_READY: int = 2;

pub const NAV_PLAN_UNREACHABLE: int = 3;

pub const NAV_Q_IDLE: int = 0;

pub const NAV_Q_START: int = 1;

pub const NAV_Q_GOAL: int = 2;

pub const NAV_Q_CHECK: int = 3;

pub const NAV_Q_SEARCH: int = 4;

pub const NAV_Q_MATERIALIZE: int = 5;

pub const NAV_Q_CERTIFY: int = 6;

pub const NAV_SHORTCUT_WINDOW: int = 8;

pub const NAV_AREA_ROUTE_LIMIT: int = 3;

pub const NAV_AREA_CACHE_LIMIT: int = 128;

#[derive(Clone, Debug)]
pub struct NavGraph {
    pub cluster_size: int,
    pub per_side: int,
    pub k: int,
    pub span: int,
    pub space_id: IdxVec<int>,
    pub space_ox: IdxVec<int>,
    pub space_oy: IdxVec<int>,
    pub space_tile_base: IdxVec<int>,
    pub space_cluster_base: IdxVec<int>,
    pub cell_space: BTreeMap<int, int>,
    pub id_space: BTreeMap<int, int>,
    pub blocked: IdxVec<bool>,
    pub corners: IdxVec<int>,
    pub extra: IdxVec<int>,
    pub cluster_space: IdxVec<int>,
    pub cluster_x: IdxVec<int>,
    pub cluster_y: IdxVec<int>,
    pub cluster_wx: IdxVec<int>,
    pub cluster_wy: IdxVec<int>,
    pub cluster_border: IdxVec<int>,
    pub cost: IdxVec<int>,
    pub border_vertical: IdxVec<bool>,
    pub border_a: IdxVec<int>,
    pub border_b: IdxVec<int>,
    pub border_count: IdxVec<int>,
    pub corner_base: int,
    pub ep_base: int,
    pub vertex_x: IdxVec<int>,
    pub vertex_y: IdxVec<int>,
    pub vertex_cluster: IdxVec<int>,
    pub corner_live: IdxVec<bool>,
    pub cluster_vertex: IdxVec<int>,
    pub node_x: IdxVec<int>,
    pub node_y: IdxVec<int>,
    pub gap_lo: IdxVec<int>,
    pub gap_hi: IdxVec<int>,
    pub component: IdxVec<int>,
    pub cluster_version: IdxVec<int>,
    pub epoch: int,
    pub link_cost: IdxVec<int>,
    pub link_bit: IdxVec<int>,
    pub tile_link_head: IdxVec<int>,
    pub link_next: IdxVec<int>,
    pub link_from_tile: IdxVec<int>,
    pub link_to_tile: IdxVec<int>,
    pub ep_cluster: IdxVec<int>,
    pub ep_index: IdxVec<int>,
    pub ep_row: IdxVec<int>,
    pub ep_cost: IdxVec<int>,
    pub cluster_ep_start: IdxVec<int>,
    pub cluster_ep_count: IdxVec<int>,
    pub cluster_eps: IdxVec<int>,
    pub ep_blocked: IdxVec<bool>,
    pub pending: IdxVec<bool>,
    pub pending_list: IdxVec<int>,
    pub phase: int,
    pub round_clusters: IdxVec<int>,
    pub round_stamp: IdxVec<int>,
    pub round_full: IdxVec<bool>,
    pub round_dir: IdxVec<bool>,
    pub round_corner: IdxVec<bool>,
    pub round_borders: IdxVec<int>,
    pub round_border_count: int,
    pub round_vertices: IdxVec<int>,
    pub round_vertex_count: int,
    pub vertex_stamp: IdxVec<int>,
    pub border_old_x: IdxVec<int>,
    pub border_old_y: IdxVec<int>,
    pub border_old_bx: IdxVec<int>,
    pub border_old_by: IdxVec<int>,
    pub round_id: int,
    pub border_stamp: IdxVec<int>,
    pub cursor: int,
    pub cursor_slot: int,
    pub refresh_spent: int,
    pub comp_full: bool,
    pub comp_stage: int,
    pub comp_cursor: int,
    pub comp_head: int,
    pub comp_tail: int,
    pub comp_stamp: int,
    pub comp_visit: IdxVec<int>,
    pub comp_group: IdxVec<int>,
    pub flood_parent: IdxVec<int>,
    pub flood_open: IdxVec<int>,
    pub flood_head: IdxVec<int>,
    pub flood_tail: IdxVec<int>,
    pub flood_claims: IdxVec<int>,
    pub flood_label: IdxVec<int>,
    pub flood_count: int,
    pub comp_unfinished: int,
    pub comp_seeds: int,
    pub comp_checked: int,
    pub comp_run: IdxVec<int>,
    pub comp_run_at: int,
    pub comp_next: IdxVec<int>,
    pub comp_merge: bool,
    pub comp_touched: IdxVec<int>,
    pub claim_label: IdxVec<int>,
    pub claim_next: IdxVec<int>,
    pub claim_count: int,
    pub comp_check_id: int,
    pub comp_check_active: bool,
    pub comp_check_flood: int,
    pub comp_check_claim: int,
    pub comp_check_apart: bool,
    pub label_size: IdxVec<int>,
    pub label_stamp: IdxVec<int>,
    pub label_owner: IdxVec<int>,
    pub label_owner_stamp: IdxVec<int>,
    pub label_free: IdxVec<int>,
    pub search_stamp: int,
    pub search_cluster: int,
    pub search_uniform: int,
    pub search_edges: IdxVec<Option<int>>,
    pub search_ready: IdxVec<bool>,
    pub dist: IdxVec<int>,
    pub dist_stamp: IdxVec<int>,
    pub open_cost: IdxVec<int>,
    pub open_tile: IdxVec<int>,
    pub open_slot: IdxVec<int>,
    pub queue: IdxVec<int>,
}

impl Default for NavGraph {
    fn default() -> Self {
        Self {
            cluster_size: NAV_CLUSTER_SIZE_DEFAULT,
            per_side: 0,
            k: 0,
            span: 0,
            space_id: IdxVec::default(),
            space_ox: IdxVec::default(),
            space_oy: IdxVec::default(),
            space_tile_base: IdxVec::default(),
            space_cluster_base: IdxVec::default(),
            cell_space: BTreeMap::new(),
            id_space: BTreeMap::new(),
            blocked: IdxVec::default(),
            corners: IdxVec::default(),
            extra: IdxVec::default(),
            cluster_space: IdxVec::default(),
            cluster_x: IdxVec::default(),
            cluster_y: IdxVec::default(),
            cluster_wx: IdxVec::default(),
            cluster_wy: IdxVec::default(),
            cluster_border: IdxVec::default(),
            cost: IdxVec::default(),
            border_vertical: IdxVec::default(),
            border_a: IdxVec::default(),
            border_b: IdxVec::default(),
            border_count: IdxVec::default(),
            corner_base: 0,
            ep_base: 0,
            vertex_x: IdxVec::default(),
            vertex_y: IdxVec::default(),
            vertex_cluster: IdxVec::default(),
            corner_live: IdxVec::default(),
            cluster_vertex: IdxVec::default(),
            node_x: IdxVec::default(),
            node_y: IdxVec::default(),
            gap_lo: IdxVec::default(),
            gap_hi: IdxVec::default(),
            component: IdxVec::default(),
            cluster_version: IdxVec::default(),
            epoch: 0,
            link_cost: IdxVec::default(),
            link_bit: IdxVec::default(),
            tile_link_head: IdxVec::default(),
            link_next: IdxVec::default(),
            link_from_tile: IdxVec::default(),
            link_to_tile: IdxVec::default(),
            ep_cluster: IdxVec::default(),
            ep_index: IdxVec::default(),
            ep_row: IdxVec::default(),
            ep_cost: IdxVec::default(),
            cluster_ep_start: IdxVec::default(),
            cluster_ep_count: IdxVec::default(),
            cluster_eps: IdxVec::default(),
            ep_blocked: IdxVec::default(),
            pending: IdxVec::default(),
            pending_list: IdxVec::default(),
            phase: NAV_PHASE_IDLE,
            round_clusters: IdxVec::default(),
            round_stamp: IdxVec::default(),
            round_full: IdxVec::default(),
            round_dir: IdxVec::default(),
            round_corner: IdxVec::default(),
            round_borders: IdxVec::default(),
            round_border_count: 0,
            round_vertices: IdxVec::default(),
            round_vertex_count: 0,
            vertex_stamp: IdxVec::default(),
            border_old_x: IdxVec::default(),
            border_old_y: IdxVec::default(),
            border_old_bx: IdxVec::default(),
            border_old_by: IdxVec::default(),
            round_id: 0,
            border_stamp: IdxVec::default(),
            cursor: 0,
            cursor_slot: 0,
            refresh_spent: 0,
            comp_full: true,
            comp_stage: NAV_COMP_DETACH,
            comp_cursor: 0,
            comp_head: 0,
            comp_tail: 0,
            comp_stamp: 0,
            comp_visit: IdxVec::default(),
            comp_group: IdxVec::default(),
            flood_parent: IdxVec::default(),
            flood_open: IdxVec::default(),
            flood_head: IdxVec::default(),
            flood_tail: IdxVec::default(),
            flood_claims: IdxVec::default(),
            flood_label: IdxVec::default(),
            flood_count: 0,
            comp_unfinished: 0,
            comp_seeds: 0,
            comp_checked: 0,
            comp_run: IdxVec::default(),
            comp_run_at: 0,
            comp_next: IdxVec::default(),
            comp_merge: false,
            comp_touched: IdxVec::default(),
            claim_label: IdxVec::default(),
            claim_next: IdxVec::default(),
            claim_count: 0,
            comp_check_id: 0,
            comp_check_active: false,
            comp_check_flood: 0,
            comp_check_claim: -1,
            comp_check_apart: true,
            label_size: IdxVec::default(),
            label_stamp: IdxVec::default(),
            label_owner: IdxVec::default(),
            label_owner_stamp: IdxVec::default(),
            label_free: IdxVec::default(),
            search_stamp: 0,
            search_cluster: -1,
            search_uniform: -1,
            search_edges: IdxVec::default(),
            search_ready: IdxVec::default(),
            dist: IdxVec::default(),
            dist_stamp: IdxVec::default(),
            open_cost: IdxVec::default(),
            open_tile: IdxVec::default(),
            open_slot: IdxVec::default(),
            queue: IdxVec::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct NavQuery {
    pub cert_link_visits: int,
    pub goal_searches: int,
    pub goal_hits: int,
    pub goal_fields: BTreeMap<int, BTreeMap<int, NavGoalAttachment>>,
    pub goal_field_epoch: int,
    pub goal_field_count: int,
    pub plans: IdxVec<NavPlan>,
    pub free: IdxVec<int>,
    pub head: int,
    pub tail: int,
    pub advance_spent: int,
    pub areas: NavAreas,
    pub area_prepared: bool,
    pub area_pair: int,
    pub area_building: bool,
    pub area_choice: int,
    pub area_target: int,
    pub area_goal: int,
    pub area_rank: IdxVec<int>,
    pub area_rank_stamp: IdxVec<int>,
    pub active: int,
    pub phase: int,
    pub epoch: int,
    pub start_cluster: int,
    pub gcx0: int,
    pub gcy0: int,
    pub gcx1: int,
    pub gcy1: int,
    pub gc_cursor: int,
    pub start_count: int,
    pub start_node: IdxVec<int>,
    pub start_cost: IdxVec<int>,
    pub goal_count: int,
    pub goal_node: IdxVec<int>,
    pub direct: int,
    pub best: int,
    pub best_node: int,
    pub expansions: int,
    pub stamp: int,
    pub node_g: IdxVec<int>,
    pub node_stamp: IdxVec<int>,
    pub node_parent: IdxVec<int>,
    pub node_closed: IdxVec<int>,
    pub node_link: IdxVec<int>,
    pub goal_cost: IdxVec<int>,
    pub goal_stamp: IdxVec<int>,
    pub comp_mark: IdxVec<int>,
    pub heap: NavHeap,
    pub cand_from: int,
    pub cand_count: int,
    pub cand_v: IdxVec<int>,
    pub cand_g: IdxVec<int>,
    pub cand_link: IdxVec<int>,
    pub pad_count: int,
    pub pad_x: IdxVec<int>,
    pub pad_y: IdxVec<int>,
    pub pad_cost: int,
    pub pad_h: IdxVec<int>,
    pub pad_h_stamp: IdxVec<int>,
    pub cert_dist: IdxVec<int>,
    pub cert_stamp: IdxVec<int>,
    pub cert_parent: IdxVec<int>,
    pub cert_link: IdxVec<int>,
    pub cert_closed: IdxVec<int>,
    pub tile_stamp_id: int,
    pub tile_ox: int,
    pub tile_oy: int,
    pub tile_stride: int,
    pub tile_found: int,
    pub refine_work: int,
    pub tile_dist: IdxVec<int>,
    pub tile_stamp: IdxVec<int>,
    pub tile_parent: IdxVec<int>,
    pub open_cost: IdxVec<int>,
    pub open_tile: IdxVec<int>,
    pub open_slot: IdxVec<int>,
}

impl Default for NavQuery {
    fn default() -> Self {
        Self {
            cert_link_visits: 0,
            goal_searches: 0,
            goal_hits: 0,
            goal_fields: BTreeMap::new(),
            goal_field_epoch: -1,
            goal_field_count: 0,
            plans: IdxVec::default(),
            free: IdxVec::default(),
            head: -1,
            tail: -1,
            advance_spent: 0,
            areas: NavAreas::default(),
            area_prepared: false,
            area_pair: -1,
            area_building: false,
            area_choice: -1,
            area_target: -1,
            area_goal: -1,
            area_rank: IdxVec::default(),
            area_rank_stamp: IdxVec::default(),
            active: -1,
            phase: NAV_Q_IDLE,
            epoch: 0,
            start_cluster: -1,
            gcx0: 0,
            gcy0: 0,
            gcx1: 0,
            gcy1: 0,
            gc_cursor: 0,
            start_count: 0,
            start_node: IdxVec::default(),
            start_cost: IdxVec::default(),
            goal_count: 0,
            goal_node: IdxVec::default(),
            direct: NAV_INF,
            best: NAV_INF,
            best_node: -1,
            expansions: 0,
            stamp: 0,
            node_g: IdxVec::default(),
            node_stamp: IdxVec::default(),
            node_parent: IdxVec::default(),
            node_closed: IdxVec::default(),
            node_link: IdxVec::default(),
            goal_cost: IdxVec::default(),
            goal_stamp: IdxVec::default(),
            comp_mark: IdxVec::default(),
            heap: NavHeap::default(),
            cand_from: -1,
            cand_count: 0,
            cand_v: IdxVec::default(),
            cand_g: IdxVec::default(),
            cand_link: IdxVec::default(),
            pad_count: 0,
            pad_x: IdxVec::default(),
            pad_y: IdxVec::default(),
            pad_cost: NAV_INF,
            pad_h: IdxVec::default(),
            pad_h_stamp: IdxVec::default(),
            cert_dist: IdxVec::default(),
            cert_stamp: IdxVec::default(),
            cert_parent: IdxVec::default(),
            cert_link: IdxVec::default(),
            cert_closed: IdxVec::default(),
            tile_stamp_id: 0,
            tile_ox: 0,
            tile_oy: 0,
            tile_stride: 0,
            tile_found: -1,
            refine_work: 0,
            tile_dist: IdxVec::default(),
            tile_stamp: IdxVec::default(),
            tile_parent: IdxVec::default(),
            open_cost: IdxVec::default(),
            open_tile: IdxVec::default(),
            open_slot: IdxVec::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct NavPlan {
    pub slot: int,
    pub generation: int,
    pub state: int,
    pub due_tick: int,
    pub queue_prev: int,
    pub queue_next: int,
    pub teleports: int,
    pub seed: int,
    pub si: int,
    pub from_x: int,
    pub from_y: int,
    pub goal_si: int,
    pub goal_x: int,
    pub goal_y: int,
    pub goal_w: int,
    pub goal_h: int,
    pub goal_wx: int,
    pub goal_wy: int,
    pub expansions: int,
    pub cost: int,
    pub epoch: int,
    pub wp_count: int,
    pub wp_x: IdxVec<int>,
    pub wp_y: IdxVec<int>,
    pub wp_cluster: IdxVec<int>,
    pub wp_link: IdxVec<int>,
    pub wp_next: int,
    pub final_cluster: int,
    pub last_link: int,
    pub ver_count: int,
    pub ver_cluster: IdxVec<int>,
    pub ver_value: IdxVec<int>,
    pub seg_count: int,
    pub seg_x: IdxVec<int>,
    pub seg_y: IdxVec<int>,
    pub seg_pos: int,
    pub route: IdxVec<NavRouteTile>,
    pub path_pos: int,
    pub path_cost: int,
    pub walked_cost: int,
}

impl Default for NavPlan {
    fn default() -> Self {
        Self {
            slot: 0,
            generation: 0,
            state: NAV_PLAN_FREE,
            due_tick: 0,
            queue_prev: -1,
            queue_next: -1,
            teleports: 0,
            seed: 0,
            si: 0,
            from_x: 0,
            from_y: 0,
            goal_si: 0,
            goal_x: 0,
            goal_y: 0,
            goal_w: 0,
            goal_h: 0,
            goal_wx: 0,
            goal_wy: 0,
            expansions: 0,
            cost: 0,
            epoch: 0,
            wp_count: 0,
            wp_x: IdxVec::default(),
            wp_y: IdxVec::default(),
            wp_cluster: IdxVec::default(),
            wp_link: IdxVec::default(),
            wp_next: 0,
            final_cluster: -1,
            last_link: -1,
            ver_count: 0,
            ver_cluster: IdxVec::default(),
            ver_value: IdxVec::default(),
            seg_count: 0,
            seg_x: IdxVec::default(),
            seg_y: IdxVec::default(),
            seg_pos: 0,
            route: IdxVec::default(),
            path_pos: 0,
            path_cost: 0,
            walked_cost: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NavGoalAttachment {
    pub teleports: int,
    pub nodes: IdxVec<int>,
    pub costs: IdxVec<int>,
    pub work: int,
}

#[derive(Clone, Debug)]
pub struct NavArea {
    pub id: int,
    pub nodes: IdxVec<int>,
    pub anchor: int,
}

impl Default for NavArea {
    fn default() -> Self {
        Self {
            id: -1,
            nodes: IdxVec::default(),
            anchor: -1,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NavAreaRoute {
    pub areas: IdxVec<int>,
    pub cost: int,
}

#[derive(Clone, Debug)]
pub struct NavAreaPair {
    pub from: int,
    pub to: int,
    pub teleports: int,
    pub routes: IdxVec<NavAreaRoute>,
    pub count: int,
    pub attempts: int,
    pub ready: bool,
    pub shortest: int,
    pub penalty: BTreeMap<int, int>,
}

impl Default for NavAreaPair {
    fn default() -> Self {
        Self {
            from: -1,
            to: -1,
            teleports: 0,
            routes: IdxVec::default(),
            count: 0,
            attempts: 0,
            ready: false,
            shortest: -1,
            penalty: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NavWorkCounters {
    pub requests: int,
    pub repeated_attachments: int,
    pub derivations: int,
    pub searches: int,
    pub hits: int,
    pub misses: int,
    pub evictions: int,
}

#[derive(Clone, Debug)]
pub struct NavAreas {
    pub work: Option<NavWorkCounters>,
    pub epoch: int,
    pub nodes: IdxVec<int>,
    pub areas: IdxVec<NavArea>,
    pub count: int,
    pub index: IdxVec<int>,
    pub cluster_epoch: IdxVec<int>,
    pub pairs: IdxVec<NavAreaPair>,
    pub pair_count: int,
    pub builds: int,
    pub candidate: IdxVec<int>,
}

impl Default for NavAreas {
    fn default() -> Self {
        Self {
            work: None,
            epoch: -1,
            nodes: IdxVec::default(),
            areas: IdxVec::default(),
            count: 0,
            index: IdxVec::default(),
            cluster_epoch: IdxVec::default(),
            pairs: IdxVec::default(),
            pair_count: 0,
            builds: 0,
            candidate: IdxVec::default(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NavHeap {
    pub f: IdxVec<int>,
    pub g: IdxVec<int>,
    pub n: IdxVec<int>,
    pub count: int,
}
