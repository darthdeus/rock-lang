use super::*;

/// A struct whose `Default` is each field's `= value`, or its type's default.
macro_rules! nav_struct {
    ($(#[$meta:meta])* pub struct $name:ident { $(pub $field:ident: $ty:ty $(= $value:expr)?,)* }) => {
        $(#[$meta])*
        pub struct $name {
            $(pub $field: $ty,)*
        }
        impl Default for $name {
            fn default() -> Self {
                Self { $($field: nav_struct!(@value $($value)?),)* }
            }
        }
    };
    (@value $value:expr) => { $value };
    (@value) => { Default::default() };
}

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

nav_struct! {
    #[derive(Clone, Debug)]
    pub struct NavGraph {
        pub cluster_size: int = NAV_CLUSTER_SIZE_DEFAULT,
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
        pub phase: int = NAV_PHASE_IDLE,
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
        pub border_old: IdxVec<[int; 4]>,
        pub round_id: int,
        pub border_stamp: IdxVec<int>,
        pub cursor: int,
        pub cursor_slot: int,
        pub refresh_spent: int,
        pub comp_full: bool = true,
        pub comp_stage: int = NAV_COMP_DETACH,
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
        pub comp_check_claim: int = -1,
        pub comp_check_apart: bool = true,
        pub label_size: IdxVec<int>,
        pub label_stamp: IdxVec<int>,
        pub label_owner: IdxVec<int>,
        pub label_owner_stamp: IdxVec<int>,
        pub label_free: IdxVec<int>,
        pub search_cluster: int = -1,
        pub search_uniform: int = -1,
        pub search_edges: IdxVec<Option<int>>,
        pub search_ready: IdxVec<bool>,
        pub window: WindowSearch,
        pub queue: IdxVec<int>,
    }
}

nav_struct! {
    #[derive(Clone, Debug)]
    pub struct NavQuery {
        pub cert_link_visits: int,
        pub goal_searches: int,
        pub goal_hits: int,
        pub goal_fields: BTreeMap<int, BTreeMap<int, NavGoalAttachment>>,
        pub goal_field_epoch: int = -1,
        pub goal_field_count: int,
        pub plans: IdxVec<NavPlan>,
        pub free: IdxVec<int>,
        pub head: int = -1,
        pub tail: int = -1,
        pub advance_spent: int,
        pub areas: NavAreas,
        pub area_prepared: bool,
        pub area_pair: int = -1,
        pub area_building: bool,
        pub area_choice: int = -1,
        pub area_target: int = -1,
        pub area_goal: int = -1,
        pub area_rank: IdxVec<int>,
        pub area_rank_stamp: IdxVec<int>,
        pub active: int = -1,
        pub phase: int = NAV_Q_IDLE,
        pub epoch: int,
        pub start_cluster: int = -1,
        pub gcx0: int,
        pub gcy0: int,
        pub gcx1: int,
        pub gcy1: int,
        pub gc_cursor: int,
        pub start: IdxVec<(int, int)>,
        pub goals: IdxVec<int>,
        pub direct: int = NAV_INF,
        pub best: int = NAV_INF,
        pub best_node: int = -1,
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
        pub pads: IdxVec<IVec2>,
        pub pad_cost: int = NAV_INF,
        pub pad_h: IdxVec<int>,
        pub pad_h_stamp: IdxVec<int>,
        pub cert_dist: IdxVec<int>,
        pub cert_stamp: IdxVec<int>,
        pub cert_parent: IdxVec<int>,
        pub cert_link: IdxVec<int>,
        pub cert_closed: IdxVec<int>,
        pub tile_ox: int,
        pub tile_oy: int,
        pub tile_stride: int,
        pub tile_found: int = -1,
        pub refine_work: int,
        pub window: WindowSearch,
    }
}

nav_struct! {
    #[derive(Clone, Debug)]
    pub struct NavPlan {
        pub slot: int,
        pub generation: int,
        pub state: int = NAV_PLAN_FREE,
        pub due_tick: int,
        pub queue_prev: int = -1,
        pub queue_next: int = -1,
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
        pub wp: IdxVec<Waypoint>,
        pub wp_next: int,
        pub final_cluster: int = -1,
        pub last_link: int = -1,
        pub versions: IdxVec<(int, int)>,
        pub seg: IdxVec<IVec2>,
        pub route: IdxVec<NavRouteTile>,
        pub path_pos: int,
        pub path_cost: int,
        pub walked_cost: int,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Waypoint {
    pub x: int,
    pub y: int,
    pub cluster: int,
    pub link: int,
}

#[derive(Clone, Debug, Default)]
pub struct NavGoalAttachment {
    pub teleports: int,
    pub nodes: IdxVec<(int, int)>,
    pub work: int,
}

nav_struct! {
    #[derive(Clone, Debug)]
    pub struct NavArea {
        pub id: int = -1,
        pub nodes: IdxVec<int>,
        pub anchor: int = -1,
    }
}

#[derive(Clone, Debug, Default)]
pub struct NavAreaRoute {
    pub areas: IdxVec<int>,
    pub cost: int,
}

nav_struct! {
    #[derive(Clone, Debug)]
    pub struct NavAreaPair {
        pub from: int = -1,
        pub to: int = -1,
        pub teleports: int,
        pub routes: IdxVec<NavAreaRoute>,
        pub count: int,
        pub attempts: int,
        pub ready: bool,
        pub shortest: int = -1,
        pub penalty: BTreeMap<int, int>,
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

nav_struct! {
    #[derive(Clone, Debug)]
    pub struct NavAreas {
        pub work: Option<NavWorkCounters>,
        pub epoch: int = -1,
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
}
