//! Logical image requests outlive worker completion until they are published.
use crate::image_decode::{self, Decoded};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_int;
use std::rc::{Rc, Weak};
use std::sync::Arc;

/// Required work must be published by the next flush; Deferred work is
/// published only by a pump, or by a flush once promoted to Required.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestClass {
    Required,
    Deferred,
}

struct Request {
    id: u64,
    width: u32,
    height: u32,
    result: RefCell<Option<Arc<Decoded>>>,
}

#[derive(Default)]
struct Pending {
    requests: HashMap<u64, Rc<Request>>,
    ready: VecDeque<u64>,
    /// Requests still Deferred.
    deferred: HashSet<u64>,
    budget: usize,
}

#[derive(Default)]
struct Publications {
    pending: Pending,
    workers: HashMap<u64, Weak<Request>>,
    selection_wait_us: u64,
    // Presentation cost belongs to the current host frame: resetting
    // statistics or changing the configured cap must not refund it.
    frame_uploaded_bytes: usize,
    hold: usize,
}

thread_local! {
    static PUBLICATIONS: RefCell<Publications> = RefCell::new(Publications::default());
}

fn fatal(message: impl std::fmt::Display) -> ! {
    ::log::error!(target: "IMAGE", "image publication failed: {message}");
    std::process::exit(5)
}

impl Publications {
    fn begin_frame(&mut self) {
        self.frame_uploaded_bytes = 0;
    }

    fn submit(
        &mut self,
        id: u64,
        _path: &str,
        width: u32,
        height: u32,
        class: RequestClass,
    ) -> Result<(), String> {
        let request = Rc::new(Request {
            id,
            width,
            height,
            result: RefCell::new(None),
        });
        self.workers.insert(id, Rc::downgrade(&request));
        self.pending.requests.insert(id, request);
        if class == RequestClass::Deferred {
            self.pending.deferred.insert(id);
        }
        Ok(())
    }

    /// Promote a pending Deferred request so the next flush waits for it.
    /// False when there is nothing to promote.
    fn require(&mut self, id: u64) -> bool {
        self.pending.requests.contains_key(&id) && self.pending.deferred.remove(&id)
    }

    fn ready(&self, id: u64) -> bool {
        !self.pending.requests.contains_key(&id)
    }

    /// Decoded RGBA bytes of every unpublished request, at its logical size:
    /// what the decode pool holds or will hold for them.
    fn pending_bytes(&self) -> u64 {
        self.pending
            .requests
            .values()
            .map(|r| u64::from(r.width) * u64::from(r.height) * 4)
            .sum()
    }

    fn required_unreceived(&self) -> HashSet<u64> {
        self.pending
            .requests
            .values()
            .filter(|r| !self.pending.deferred.contains(&r.id) && r.result.borrow().is_none())
            .map(|r| r.id)
            .collect()
    }

    fn retire(&mut self, id: u64) {
        self.pending.requests.remove(&id);
        self.pending.deferred.remove(&id);
    }

    fn receive(&mut self, decoded: Decoded) {
        let id = decoded.id;
        if let Some(request) = self.workers.remove(&id).and_then(|w| w.upgrade()) {
            *request.result.borrow_mut() = Some(Arc::new(decoded));
            if self.pending.requests.contains_key(&id) {
                self.pending.ready.push_back(id);
            }
        }
        // A retired request may still have a worker finishing. Its result
        // belongs to no active request and can be released.
    }

    fn collect(&mut self) {
        for decoded in image_decode::try_drain() {
            self.receive(decoded);
        }
    }

    #[cfg(test)]
    fn select(
        &mut self,
        site: &str,
        exists: impl Fn(u64) -> bool,
    ) -> Result<Vec<Arc<Decoded>>, String> {
        let mut selected = Vec::new();
        self.select_with(site, exists, |d| selected.push(d))?;
        Ok(selected)
    }

    /// Choose this site's publications and hand each to `publish`.
    /// Flushes select Required requests; pumps select either request class.
    fn select_with(
        &mut self,
        site: &str,
        exists: impl Fn(u64) -> bool,
        mut publish: impl FnMut(Arc<Decoded>),
    ) -> Result<(), String> {
        self.selection_wait_us = 0;
        if self.hold > 0 {
            return Ok(());
        }
        let budget = self.pending.budget;
        if site == "flush" || (site == "pump" && budget == 0) {
            return self.stream(site, budget, &exists, &mut publish);
        }
        self.collect();
        let selected = if site == "pump" {
            let mut ready: VecDeque<_> = self
                .pending
                .ready
                .iter()
                .filter_map(|id| {
                    self.pending
                        .requests
                        .get(id)
                        .and_then(|r| r.result.borrow().clone())
                })
                .collect();
            crate::image_texture::take_decoded_upload_budget(
                &mut ready,
                budget,
                &mut self.frame_uploaded_bytes,
            )
        } else {
            Vec::new()
        };
        for d in &selected {
            self.retire(d.id);
        }
        self.pending
            .ready
            .retain(|id| self.pending.requests.contains_key(id));
        for d in selected {
            publish(d);
        }
        Ok(())
    }

    /// Publish selected requests in readiness order. A flush retains Deferred
    /// completions and waits only for Required requests; a blocking pump
    /// publishes each completion before receiving the next one.
    fn stream(
        &mut self,
        site: &str,
        _budget: usize,
        _exists: &impl Fn(u64) -> bool,
        publish: &mut impl FnMut(Arc<Decoded>),
    ) -> Result<(), String> {
        self.collect();
        self.publish_ready(site, publish);
        if site == "flush" {
            let mut waited = std::time::Duration::ZERO;
            let mut awaited = self.required_unreceived();
            while !awaited.is_empty() {
                let start = std::time::Instant::now();
                let Some(decoded) = image_decode::recv_one() else {
                    crate::log_once!(
                        "IMAGE",
                        "flush: {} Required image request(s) have no decode outstanding",
                        awaited.len()
                    );
                    break;
                };
                waited += start.elapsed();
                awaited.remove(&decoded.id);
                self.receive(decoded);
                self.publish_ready(site, publish);
            }
            self.selection_wait_us = waited.as_micros() as u64;
        } else {
            self.selection_wait_us = image_decode::drain_with(|decoded| {
                self.receive(decoded);
                self.publish_ready(site, publish);
            });
        }
        Ok(())
    }

    fn publish_ready(&mut self, site: &str, publish: &mut impl FnMut(Arc<Decoded>)) {
        for _ in 0..self.pending.ready.len() {
            let id = self.pending.ready.pop_front().unwrap();
            if site == "flush" && self.pending.deferred.contains(&id) {
                self.pending.ready.push_back(id);
                continue;
            }
            let Some(d) = self
                .pending
                .requests
                .get(&id)
                .and_then(|r| r.result.borrow().clone())
            else {
                continue;
            };
            self.retire(id);
            // Required flushes and explicit drains bypass the cap, but their
            // uploads still spend this frame's remaining background allowance.
            self.frame_uploaded_bytes = self
                .frame_uploaded_bytes
                .saturating_add(crate::image_texture::decoded_upload_bytes(&d));
            publish(d);
        }
    }
}

pub(crate) fn submit(id: u64, path: &str, width: u32, height: u32, class: RequestClass) {
    PUBLICATIONS
        .with(|p| p.borrow_mut().submit(id, path, width, height, class))
        .unwrap_or_else(|e| fatal(e));
}

/// Drop a handle's active request, if any, as destruction does. Its bytes
/// stop counting as pending, its readiness reports published, and a flush no
/// longer waits for it.
pub(crate) fn retire(id: u64) {
    PUBLICATIONS.with(|p| p.borrow_mut().retire(id));
}

/// Publish this site's selection through `publish`, which runs inside the
/// publication state and must not re-enter it.
pub(crate) fn publish(site: &str, publish: impl FnMut(Arc<Decoded>)) {
    PUBLICATIONS
        .with(|p| {
            p.borrow_mut().select_with(
                site,
                |id| crate::image_texture::IMAGES.with(|images| images.borrow().contains_key(&id)),
                publish,
            )
        })
        .unwrap_or_else(|e| fatal(e))
}

/// Begin one host presentation frame, independently of profiler/stat resets.
#[no_mangle]
pub extern "C" fn stone_begin_image_upload_frame() {
    PUBLICATIONS.with(|p| p.borrow_mut().begin_frame());
}

pub(crate) fn set_budget(bytes: usize) {
    PUBLICATIONS.with(|p| p.borrow_mut().pending.budget = bytes);
}
pub(crate) fn pending() -> usize {
    PUBLICATIONS.with(|p| p.borrow().pending.requests.len())
}
pub(crate) fn selection_wait_us() -> c_int {
    PUBLICATIONS.with(|p| p.borrow().selection_wait_us.min(c_int::MAX as u64) as c_int)
}

/// Promote a handle's Deferred request to Required, so the next flush
/// publishes it. False when the handle has no pending Deferred request.
#[no_mangle]
pub extern "C" fn stone_image_require(handle: u64) -> bool {
    PUBLICATIONS.with(|p| p.borrow_mut().require(handle))
}

/// True when no request for the handle is waiting to be published. Says
/// nothing about whether the handle is live. A failed decode is never ready.
#[no_mangle]
pub extern "C" fn stone_image_ready(handle: u64) -> bool {
    PUBLICATIONS.with(|p| p.borrow().ready(handle))
        && !crate::image_texture::image_decode_failed(handle)
}

#[no_mangle]
pub extern "C" fn stone_image_pending_bytes() -> i64 {
    PUBLICATIONS.with(|p| i64::try_from(p.borrow().pending_bytes()).unwrap_or(i64::MAX))
}

// Stop publishing decoded images while held, so a request submitted under a
// hold stays PENDING. Balanced: an unmatched release panics.
#[no_mangle]
pub extern "C" fn stone_image_publication_hold(hold: bool) {
    PUBLICATIONS.with(|p| {
        let mut p = p.borrow_mut();
        if hold {
            p.hold += 1;
        } else {
            p.hold = p
                .hold
                .checked_sub(1)
                .expect("balanced image publication hold");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: u64, bytes: usize) -> Decoded {
        Decoded {
            id,
            pixels: Some(vec![0; bytes]),
            width: bytes as u32,
            height: 1,
            stored: (bytes as u32, 1),
            format: crate::image_texture::ImageFormat::Rgba8,
        }
    }

    fn requests() -> Publications {
        let mut p = Publications::default();
        p.pending.budget = 10;
        for id in 1..=4 {
            p.submit(id, &format!("image-{id}"), 4, 1, RequestClass::Required)
                .unwrap();
        }
        p
    }

    #[test]
    fn load_collects_without_publishing_and_preserves_live_readiness_for_flush() {
        if in_isolated_child(
            "load_collects_without_publishing_and_preserves_live_readiness_for_flush",
        ) {
            return;
        }
        let mut p = requests();
        p.receive(result(3, 4));
        p.receive(result(1, 4));
        p.retire(1);
        let (tx, _notice) = image_decode::test_completions(2);
        tx.send(result(4, 4)).unwrap();
        tx.send(result(2, 4)).unwrap();

        let mut published = Vec::new();
        p.select_with("load", |_| true, |d| published.push(d.id))
            .unwrap();
        assert!(published.is_empty(), "load must not publish decoded images");
        assert_eq!(
            p.pending.ready.len(),
            3,
            "load retains all live completions and prunes the retired one"
        );
        assert!(!p.pending.ready.contains(&1), "retired readiness is pruned");
        assert_eq!(
            image_decode::outstanding_count(),
            0,
            "load collects worker completions"
        );

        p.select_with("flush", |_| true, |d| published.push(d.id))
            .unwrap();
        assert_eq!(
            published,
            [3, 4, 2],
            "flush follows the live completion arrival order"
        );
        assert!(p.pending.requests.is_empty());
        assert!(p.pending.ready.is_empty());
    }

    /// A flush must not hold every decoded buffer at once: each completion is
    /// published (uploaded) before the next one is received, in the same order
    /// and with the same journal batch as a drain-then-publish flush.
    #[test]
    fn flush_publishes_each_completion_before_receiving_the_next() {
        if in_isolated_child("flush_publishes_each_completion_before_receiving_the_next") {
            return;
        }
        let mut p = requests();
        p.receive(result(1, 4));
        let (tx, _notice) = image_decode::test_completions(3);
        let (published, seen) = std::sync::mpsc::channel::<u64>();
        let worker = std::thread::spawn(move || {
            let mut streamed = Vec::new();
            for id in [3, 2, 4] {
                tx.send(result(id, 4)).unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                let mut found = false;
                while let Ok(got) =
                    seen.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                {
                    if got == id {
                        found = true;
                        break;
                    }
                }
                streamed.push(found);
            }
            streamed
        });
        let mut order = Vec::new();
        p.select_with(
            "flush",
            |_| true,
            |d| {
                order.push(d.id);
                let _ = published.send(d.id);
            },
        )
        .unwrap();
        assert_eq!(
            worker.join().unwrap(),
            [true, true, true],
            "each completion must be published before the next is received"
        );
        assert_eq!(order, [1, 3, 2, 4]);
        assert!(p.pending.requests.is_empty());
        assert_eq!(image_decode::outstanding_count(), 0);
    }

    #[test]
    fn neither_a_statistics_reset_nor_a_budget_change_refunds_frame_uploads() {
        PUBLICATIONS.with(|p| {
            let mut p = p.borrow_mut();
            *p = requests();
            for id in 1..=4 {
                p.receive(result(id, 4));
            }
            assert_eq!(
                p.select("pump", |_| true).unwrap().len(),
                2,
                "two 4-byte images fit the 10-byte frame budget"
            );
        });
        crate::stone_reset_frame_stats();
        set_budget(10);
        PUBLICATIONS.with(|p| {
            assert!(
                p.borrow_mut().select("pump", |_| true).unwrap().is_empty(),
                "neither statistics reset nor setting a budget refunds uploads"
            )
        });
        stone_begin_image_upload_frame();
        PUBLICATIONS.with(|p| {
            let mut p = p.borrow_mut();
            assert_eq!(p.select("pump", |_| true).unwrap().len(), 2);
            assert!(p.pending.requests.is_empty());
        });
        PUBLICATIONS.with(|p| *p.borrow_mut() = Publications::default());
    }

    /// Fixtures that feed the process-wide decode queue run alone in a child
    /// process, since other fixtures drain that queue. Returns true in the
    /// parent once the child has passed; the child returns false and runs.
    fn in_isolated_child(name: &str) -> bool {
        const CHILD: &str = "STONE_IMAGE_PUBLICATION_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                &format!("image_publication::tests::{name}"),
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("run isolated publication fixture");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "the child must select {name}"
        );
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn classed(classes: &[RequestClass]) -> Publications {
        let mut p = Publications::default();
        p.pending.budget = 10;
        for (i, class) in classes.iter().enumerate() {
            let id = i as u64 + 1;
            p.submit(id, &format!("image-{id}"), 4, 1, *class).unwrap();
        }
        p
    }

    /// A completion the worker sends only when told to, or after a timeout so
    /// a flush that waits for it cannot hang the fixture. Reports whether it
    /// was released by the signal rather than the timeout.
    fn release_on_signal(
        tx: std::sync::mpsc::Sender<Decoded>,
        id: u64,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<bool>) {
        let (signal, signalled) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let released = signalled
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok();
            tx.send(result(id, 4)).unwrap();
            released
        });
        (signal, worker)
    }

    #[test]
    fn flush_returns_once_required_work_is_published_and_leaves_deferred_work_pending() {
        if in_isolated_child(
            "flush_returns_once_required_work_is_published_and_leaves_deferred_work_pending",
        ) {
            return;
        }
        let mut p = classed(&[RequestClass::Deferred, RequestClass::Required]);
        let (tx, _notice) = image_decode::test_completions(2);
        tx.send(result(2, 4)).unwrap();
        let (signal, deferred_worker) = release_on_signal(tx, 1);
        let published: Vec<u64> = p
            .select("flush", |_| true)
            .unwrap()
            .iter()
            .map(|d| d.id)
            .collect();
        let _ = signal.send(());
        assert!(
            deferred_worker.join().unwrap(),
            "the flush waited for the Deferred decode"
        );
        assert_eq!(
            published,
            [2],
            "the flush publishes the Required request only"
        );
        assert!(p.ready(2));
        assert!(!p.ready(1), "the Deferred request stays pending");
        assert_eq!(p.pending_bytes(), 16);
        p.receive(image_decode::wait_one().unwrap());
        assert_eq!(
            p.select("pump", |_| true)
                .unwrap()
                .iter()
                .map(|d| d.id)
                .collect::<Vec<_>>(),
            [1]
        );
        assert!(p.ready(1));
        assert_eq!(p.pending_bytes(), 0);
    }

    #[test]
    fn flush_leaves_received_deferred_results_for_the_budgeted_pump() {
        if in_isolated_child("flush_leaves_received_deferred_results_for_the_budgeted_pump") {
            return;
        }
        let mut p = classed(&[
            RequestClass::Deferred,
            RequestClass::Required,
            RequestClass::Deferred,
            RequestClass::Required,
        ]);
        p.receive(result(1, 4));
        p.receive(result(2, 4));
        p.hold = 1;
        assert!(
            p.select("flush", |_| true).unwrap().is_empty(),
            "a publication hold also blocks Required flushes"
        );
        assert_eq!(p.frame_uploaded_bytes, 0);
        p.hold = 0;
        let (tx, _notice) = image_decode::test_completions(2);
        let mut order = Vec::new();
        p.select_with(
            "flush",
            |_| true,
            |d| {
                order.push(d.id);
                if d.id == 2 {
                    // These complete after the initial collection, while a
                    // Required request still keeps the flush open.
                    tx.send(result(3, 4)).unwrap();
                    tx.send(result(4, 4)).unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(
            order,
            [2, 4],
            "only Required results publish at either receive site"
        );
        assert!(!p.ready(1));
        assert!(!p.ready(3));
        assert_eq!(p.frame_uploaded_bytes, 8);
        assert!(
            p.select("pump", |_| true).unwrap().is_empty(),
            "the Required uploads spent this frame's allowance"
        );
        p.begin_frame();
        assert_eq!(
            p.select("pump", |_| true)
                .unwrap()
                .iter()
                .map(|d| d.id)
                .collect::<Vec<_>>(),
            [1, 3],
            "Deferred results retain their readiness order for the next frame's pump"
        );
        assert!(p.pending.requests.is_empty());
        assert_eq!(image_decode::outstanding_count(), 0);
    }

    #[test]
    fn blocking_pump_drains_both_request_classes_in_readiness_order() {
        if in_isolated_child("blocking_pump_drains_both_request_classes_in_readiness_order") {
            return;
        }
        let mut p = classed(&[
            RequestClass::Deferred,
            RequestClass::Required,
            RequestClass::Deferred,
        ]);
        p.pending.budget = 0;
        p.receive(result(2, 4));
        let (tx, _notice) = image_decode::test_completions(2);
        let worker = std::thread::spawn(move || {
            tx.send(result(1, 4)).unwrap();
            tx.send(result(3, 4)).unwrap();
        });
        let published = p.select("pump", |_| true).unwrap();
        worker.join().unwrap();
        assert_eq!(
            published.iter().map(|d| d.id).collect::<Vec<_>>(),
            [2, 1, 3]
        );
        assert!(p.pending.requests.is_empty());
        assert_eq!(image_decode::outstanding_count(), 0);
    }

    #[test]
    fn require_turns_a_deferred_request_into_a_flush_wait() {
        if in_isolated_child("require_turns_a_deferred_request_into_a_flush_wait") {
            return;
        }
        let mut p = classed(&[RequestClass::Deferred, RequestClass::Deferred]);
        p.receive(result(1, 4));
        let (tx, _notice) = image_decode::test_completions(1);
        let (signal, worker) = release_on_signal(tx, 2);
        assert!(
            p.select("flush", |_| true).unwrap().is_empty(),
            "a flush does not wait for Deferred work"
        );
        assert!(p.require(1), "a received Deferred request is promoted");
        assert!(p.require(2), "an outstanding Deferred request is promoted");
        assert!(!p.require(1), "a Required request has nothing to promote");
        let _ = signal.send(());
        assert_eq!(
            p.select("flush", |_| true)
                .unwrap()
                .iter()
                .map(|d| d.id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(worker.join().unwrap());
        assert!(p.ready(1));
        assert!(!p.require(1), "a published request has nothing to promote");
    }

    #[test]
    fn pumps_share_one_frame_budget_with_progress_for_an_oversized_image() {
        if in_isolated_child("pumps_share_one_frame_budget_with_progress_for_an_oversized_image") {
            return;
        }
        let mut p = classed(&[
            RequestClass::Deferred,
            RequestClass::Required,
            RequestClass::Deferred,
            RequestClass::Deferred,
        ]);
        p.receive(result(1, 12));
        p.receive(result(2, 4));
        let sizes = |selected: Vec<Arc<Decoded>>| {
            selected
                .iter()
                .map(|d| d.pixels.as_ref().unwrap().len())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            sizes(p.select("pump", |_| true).unwrap()),
            [12],
            "one item over the budget still publishes alone"
        );
        assert!(
            p.select("pump", |_| true).unwrap().is_empty(),
            "a second pump in the same frame cannot renew the spent allowance"
        );
        p.begin_frame();
        // Completions arriving between pump sites share the same remainder.
        assert_eq!(sizes(p.select("pump", |_| true).unwrap()), [4]);
        p.receive(result(3, 4));
        assert_eq!(sizes(p.select("pump", |_| true).unwrap()), [4]);
        p.receive(result(4, 4));
        assert!(
            p.select("pump", |_| true).unwrap().is_empty(),
            "an image larger than the remainder waits for the next frame"
        );
        p.begin_frame();
        assert_eq!(sizes(p.select("pump", |_| true).unwrap()), [4]);
        p.submit(5, "required", 4, 1, RequestClass::Required)
            .unwrap();
        p.receive(result(5, 12));
        assert_eq!(
            sizes(p.select("flush", |_| true).unwrap()),
            [12],
            "Required flushes publish even when they exceed the frame cap"
        );
        p.submit(6, "deferred", 4, 1, RequestClass::Deferred)
            .unwrap();
        p.receive(result(6, 4));
        assert!(
            p.select("pump", |_| true).unwrap().is_empty(),
            "a flush also spends the background allowance"
        );
        p.pending.budget = 0;
        assert_eq!(
            sizes(p.select("pump", |_| true).unwrap()),
            [4],
            "an explicitly unbounded pump still drains within the same frame"
        );
        assert!(p.pending.requests.is_empty());
    }

    #[test]
    fn destroying_a_handle_retires_its_request_so_no_flush_waits_for_it() {
        if in_isolated_child("destroying_a_handle_retires_its_request_so_no_flush_waits_for_it") {
            return;
        }
        PUBLICATIONS
            .with(|p| *p.borrow_mut() = classed(&[RequestClass::Required, RequestClass::Deferred]));
        let (tx, _notice) = image_decode::test_completions(2);
        assert_eq!(stone_image_pending_bytes(), 32);
        assert!(!stone_image_ready(1));
        // Through the public destroy path, as the FFI probe does.
        crate::image_texture::stone_unload_image(1);
        assert_eq!(
            stone_image_pending_bytes(),
            16,
            "a destroyed request stops counting its bytes"
        );
        assert!(
            stone_image_ready(1),
            "a destroyed request reports published"
        );
        PUBLICATIONS.with(|p| {
            assert!(
                p.borrow().required_unreceived().is_empty(),
                "a destroyed Required request must not hold a flush"
            )
        });
        PUBLICATIONS.with(|p| p.borrow_mut().select("flush", |_| true).unwrap());
        // A completion that arrives after the destroy belongs to no active
        // request; it must not resurrect the retired one.
        tx.send(result(1, 4)).unwrap();
        PUBLICATIONS.with(|p| {
            let mut p = p.borrow_mut();
            assert!(p.select("pump", |_| true).unwrap().is_empty());
            assert_eq!(p.pending_bytes(), 16);
        });
    }
}
