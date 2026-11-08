use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::Instant;

#[derive(Clone)]
pub(super) struct QueryResult {
    pub plan: NavPlan,
    pub work: int,
    pub worker_ms: f64,
    pub cert_link_visits: int,
    pub area_work: NavWorkCounters,
    #[cfg(test)]
    pub worker_thread: std::thread::ThreadId,
}
pub(super) struct QueryJob {
    pub generation: int,
    pub epoch: int,
    pub cancelled: AtomicBool,
    finished: AtomicBool,
    result: Mutex<Option<Result<QueryResult, String>>>,
    wake: Condvar,
}
impl QueryJob {
    pub fn submit(graph: Arc<NavGraph>, plan: NavPlan) -> Arc<Self> {
        let job = Arc::new(Self {
            generation: plan.generation,
            epoch: graph.epoch,
            cancelled: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            result: Mutex::new(None),
            wake: Condvar::new(),
        });
        let mut live = live_jobs().lock().unwrap();
        live.retain(|job| job.strong_count() > 0);
        live.push(Arc::downgrade(&job));
        drop(live);
        let worker = Arc::clone(&job);
        rayon::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                solve(&graph, plan, &worker.cancelled)
            }))
            .map_err(|panic| {
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "navigation worker panicked".to_string())
            });
            {
                let mut slot = worker.result.lock().unwrap();
                *slot = Some(result);
                worker.finished.store(true, Ordering::Release);
            }
            worker.wake.notify_all();
        });
        job
    }
    pub fn take(&self, wait: bool) -> Option<Result<QueryResult, String>> {
        let mut result = self.result.lock().unwrap();
        while wait && result.is_none() && !self.finished.load(Ordering::Acquire) {
            result = self.wake.wait(result).unwrap();
        }
        result.take()
    }
    pub fn wait_finished(&self) {
        let mut result = self.result.lock().unwrap();
        while !self.finished.load(Ordering::Acquire) {
            result = self.wake.wait(result).unwrap();
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
fn solve(graph: &NavGraph, mut plan: NavPlan, cancelled: &AtomicBool) -> QueryResult {
    let started = Instant::now();
    let slot = plan.slot;
    if cancelled.load(Ordering::Acquire) {
        return QueryResult {
            plan,
            work: 0,
            worker_ms: 0.0,
            cert_link_visits: 0,
            area_work: NavWorkCounters::default(),
            #[cfg(test)]
            worker_thread: std::thread::current().id(),
        };
    }
    plan.slot = 0;
    plan.queue_prev = -1;
    plan.queue_next = -1;
    let mut query = NavQuery::default();
    nav_query_size(graph, &mut query);
    query.head = 0;
    query.tail = 0;
    query.areas.work = Some(NavWorkCounters::default());
    query.plans.push(plan);
    let mut work = 0;
    while !cancelled.load(Ordering::Acquire) {
        let done = nav_graph_advance(graph, &mut query, 4096);
        work += query.advance_spent;
        if done {
            break;
        }
    }
    let mut plan = std::mem::take(&mut query.plans[0]);
    plan.slot = slot;
    QueryResult {
        plan,
        work,
        worker_ms: started.elapsed().as_secs_f64() * 1000.0,
        cert_link_visits: query.cert_link_visits,
        area_work: query.areas.work.take().unwrap(),
        #[cfg(test)]
        worker_thread: std::thread::current().id(),
    }
}

static LIVE_JOBS: OnceLock<Mutex<Vec<Weak<QueryJob>>>> = OnceLock::new();
fn live_jobs() -> &'static Mutex<Vec<Weak<QueryJob>>> {
    LIVE_JOBS.get_or_init(|| Mutex::new(Vec::new()))
}
pub(super) fn nav_jobs_shutdown() {
    let jobs: Vec<_> = live_jobs()
        .lock()
        .unwrap()
        .drain(..)
        .filter_map(|job| job.upgrade())
        .collect();
    for job in &jobs {
        job.cancel();
    }
    for job in jobs {
        job.wait_finished();
    }
}
