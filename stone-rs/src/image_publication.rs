use crate::image_decode::{self, Decoded};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_int;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestClass {
    Required,
    Deferred,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicationSite {
    Load,
    Flush,
    Pump,
}

struct Request {
    width: u32,
    height: u32,
    class: RequestClass,
    result: Option<Decoded>,
}

#[derive(Default)]
struct Publications {
    requests: HashMap<u64, Request>,
    ready: VecDeque<u64>,
    budget: usize,
    time_budget: Duration,
    fixed_work: bool,
    selection_wait_us: u64,
    frame_uploaded_bytes: usize,
    frame_upload_time: Duration,
}

thread_local! {
    static PUBLICATIONS: RefCell<Publications> = RefCell::new(Publications {
        fixed_work: image_decode::fixed_perf_work(),
        ..Publications::default()
    });
}

impl Publications {
    fn set_time_budget(&mut self, duration: Duration) {
        self.time_budget = if self.fixed_work {
            Duration::ZERO
        } else {
            duration
        };
    }

    fn begin_frame(&mut self) {
        self.frame_uploaded_bytes = 0;
        self.frame_upload_time = Duration::ZERO;
    }

    fn submit(&mut self, id: u64, width: u32, height: u32, class: RequestClass) {
        self.requests.insert(
            id,
            Request {
                width,
                height,
                class,
                result: None,
            },
        );
    }

    fn require(&mut self, id: u64) -> bool {
        let Some(request) = self.requests.get_mut(&id) else {
            return false;
        };
        std::mem::replace(&mut request.class, RequestClass::Required) == RequestClass::Deferred
    }

    fn ready(&self, id: u64) -> bool {
        !self.requests.contains_key(&id)
    }

    fn pending_bytes(&self) -> u64 {
        self.requests
            .values()
            .map(|r| u64::from(r.width) * u64::from(r.height) * 4)
            .sum()
    }

    fn required_unreceived(&self) -> usize {
        self.requests
            .values()
            .filter(|r| r.class == RequestClass::Required && r.result.is_none())
            .count()
    }

    fn retire(&mut self, id: u64) {
        self.requests.remove(&id);
    }

    fn receive(&mut self, decoded: Decoded) -> bool {
        // Handles are minted once; a completion with no request belongs to a retired image.
        let id = decoded.id;
        let Some(request) = self.requests.get_mut(&id) else {
            return false;
        };
        if request.result.is_some() {
            return false;
        }
        request.result = Some(decoded);
        self.ready.push_back(id);
        request.class == RequestClass::Required
    }

    #[cfg(test)]
    fn select(&mut self, site: PublicationSite) -> Vec<Decoded> {
        let mut selected = Vec::new();
        self.select_with(site, |d| selected.push(d));
        selected
    }

    fn select_with(&mut self, site: PublicationSite, mut publish: impl FnMut(Decoded)) {
        self.select_timed_with(site, |decoded| {
            let started = Instant::now();
            publish(decoded);
            started.elapsed()
        });
    }

    fn select_timed_with(
        &mut self,
        site: PublicationSite,
        mut publish: impl FnMut(Decoded) -> Duration,
    ) {
        self.selection_wait_us = 0;
        for decoded in image_decode::try_drain() {
            self.receive(decoded);
        }
        if site == PublicationSite::Load {
            self.ready.retain(|id| self.requests.contains_key(id));
            return;
        }
        self.publish_ready(site, &mut publish);
        if site == PublicationSite::Flush {
            let mut waited = Duration::ZERO;
            let mut awaited = self.required_unreceived();
            while awaited > 0 {
                let start = Instant::now();
                let Some(decoded) = image_decode::recv_one() else {
                    crate::log_once!(
                        "IMAGE",
                        "flush: {} Required image request(s) have no decode outstanding",
                        awaited
                    );
                    break;
                };
                waited += start.elapsed();
                awaited -= usize::from(self.receive(decoded));
                self.publish_ready(site, &mut publish);
            }
            self.selection_wait_us = waited.as_micros() as u64;
        } else if self.budget == 0 {
            self.selection_wait_us = image_decode::drain_with(|decoded| {
                self.receive(decoded);
                self.publish_ready(site, &mut publish);
            });
        }
    }

    fn publish_ready(
        &mut self,
        site: PublicationSite,
        publish: &mut impl FnMut(Decoded) -> Duration,
    ) {
        let budgeted = site == PublicationSite::Pump && self.budget > 0;
        for _ in 0..self.ready.len() {
            let id = self.ready[0];
            let Some(request) = self.requests.get_mut(&id) else {
                self.ready.pop_front();
                continue;
            };
            if site == PublicationSite::Flush && request.class == RequestClass::Deferred {
                self.ready.rotate_left(1);
                continue;
            }
            let bytes =
                crate::image_texture::decoded_upload_bytes(request.result.as_ref().unwrap());
            let uploaded = self.frame_uploaded_bytes.saturating_add(bytes);
            if budgeted
                && ((!self.time_budget.is_zero() && self.frame_upload_time >= self.time_budget)
                    || (self.frame_uploaded_bytes > 0 && uploaded > self.budget))
            {
                break;
            }
            self.ready.pop_front();
            let decoded = request.result.take().unwrap();
            self.retire(id);
            self.frame_uploaded_bytes = uploaded;
            self.frame_upload_time += publish(decoded);
        }
    }
}

pub(crate) fn submit(id: u64, width: u32, height: u32, class: RequestClass) {
    PUBLICATIONS.with(|p| p.borrow_mut().submit(id, width, height, class));
}

pub(crate) fn retire(id: u64) {
    PUBLICATIONS.with(|p| p.borrow_mut().retire(id));
}

/// The publication callback runs inside the publication state and must not re-enter it.
pub(crate) fn publish(site: PublicationSite, publish: impl FnMut(Decoded)) {
    PUBLICATIONS.with(|p| p.borrow_mut().select_with(site, publish));
}

/// Begin one host presentation frame, independently of profiler/stat resets.
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_begin_image_upload_frame() {
    PUBLICATIONS.with(|p| p.borrow_mut().begin_frame());
}

pub(crate) fn set_budget(bytes: usize) {
    PUBLICATIONS.with(|p| p.borrow_mut().budget = bytes);
}
/// rock-ffi stone
#[no_mangle]
pub extern "C" fn stone_set_frame_image_upload_time_budget(microseconds: c_int) {
    PUBLICATIONS.with(|p| {
        p.borrow_mut()
            .set_time_budget(Duration::from_micros(microseconds.max(0) as u64));
    });
}

pub(crate) fn pending() -> usize {
    PUBLICATIONS.with(|p| p.borrow().requests.len())
}
pub(crate) fn selection_wait_us() -> c_int {
    PUBLICATIONS.with(|p| p.borrow().selection_wait_us.min(c_int::MAX as u64) as c_int)
}

/// Promote a handle's Deferred request to Required, so the next flush
/// publishes it. False when the handle has no pending Deferred request.
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_image_require(handle: u64) -> bool {
    PUBLICATIONS.with(|p| p.borrow_mut().require(handle))
}

/// True when no request for the handle is waiting to be published. Says
/// nothing about whether the handle is live. A failed decode is never ready.
/// True when no load for the handle waits to be published (not a liveness test).
/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_image_ready(handle: u64) -> bool {
    PUBLICATIONS.with(|p| p.borrow().ready(handle))
        && !crate::image_texture::image_decode_failed(handle)
}

/// rock-ffi stone private
#[no_mangle]
pub extern "C" fn stone_image_pending_bytes() -> i64 {
    PUBLICATIONS.with(|p| i64::try_from(p.borrow().pending_bytes()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{PublicationSite::*, *};

    fn isolated(test: &str) -> bool {
        crate::test_support::in_isolated_child(
            &format!("image_publication::tests::{test}"),
            "STONE_IMAGE_PUBLICATION_TEST_CHILD",
        )
    }

    fn ids(selected: &[Decoded]) -> Vec<u64> {
        selected.iter().map(|d| d.id).collect()
    }

    fn result(id: u64, bytes: usize) -> Decoded {
        Decoded {
            id,
            pixels: Some(image_decode::PackedPixels {
                logical: (bytes as u32, 1),
                stored: (bytes as u32, 1),
                format: crate::image_texture::ImageFormat::Rgba8,
                data: vec![0; bytes],
                offset: 0,
            }),
        }
    }

    fn requests() -> Publications {
        let mut p = Publications {
            budget: 10,
            ..Publications::default()
        };
        for id in 1..=4 {
            p.submit(id, 4, 1, RequestClass::Required);
        }
        p
    }

    #[test]
    fn fixed_work_preserves_the_byte_quota_across_pumps() {
        if isolated("fixed_work_preserves_the_byte_quota_across_pumps") {
            return;
        }
        let mut p = Publications {
            fixed_work: true,
            ..requests()
        };
        p.set_time_budget(Duration::from_millis(2));
        for id in 1..=4 {
            p.receive(result(id, 4));
        }
        let mut published = Vec::new();
        let mut publish = |d: Decoded| {
            published.push(d.id);
            Duration::from_millis(3)
        };
        p.select_timed_with(Pump, &mut publish);
        p.select_timed_with(Pump, &mut publish);
        assert!(p.ready(1) && p.ready(2) && !p.ready(3));
        p.begin_frame();
        p.select_timed_with(Pump, &mut publish);
        assert_eq!(published, [1, 2, 3, 4]);
    }

    #[test]
    fn time_allowance_is_shared_by_pumps_and_required_flushes() {
        if isolated("time_allowance_is_shared_by_pumps_and_required_flushes") {
            return;
        }
        let mut p = Publications {
            budget: 100,
            time_budget: Duration::from_millis(2),
            ..Publications::default()
        };
        for id in 1..=4 {
            p.submit(id, 1, 1, RequestClass::Deferred);
            p.receive(result(id, 4));
        }
        let mut published = Vec::new();
        let mut publish = |d: Decoded| {
            published.push(d.id);
            Duration::from_millis(3)
        };
        p.select_timed_with(Pump, &mut publish);
        assert!(p.ready(1));
        assert!(
            !p.ready(2),
            "stop after the first image exceeds the time allowance"
        );
        p.select_timed_with(Pump, &mut publish);
        assert!(
            !p.ready(2),
            "another pump cannot refund elapsed publication time"
        );
        p.require(2);
        p.select_timed_with(Flush, &mut publish);
        assert!(
            p.ready(2),
            "Required images still publish after the allowance is spent"
        );
        assert!(!p.ready(3));
        p.select_timed_with(Pump, &mut publish);
        assert!(!p.ready(3));
        p.begin_frame();
        p.select_timed_with(Pump, &mut publish);
        assert!(p.ready(3));
        assert!(!p.ready(4));
        p.budget = 0;
        p.select_timed_with(Pump, &mut publish);
        assert!(p.ready(4), "explicit loading drains remain unbounded");
        assert_eq!(published, [1, 2, 3, 4]);
    }

    #[test]
    fn load_collects_without_publishing_and_preserves_live_readiness_for_flush() {
        if isolated("load_collects_without_publishing_and_preserves_live_readiness_for_flush") {
            return;
        }
        let mut p = requests();
        p.receive(result(3, 4));
        p.receive(result(1, 4));
        p.retire(1);
        let tx = image_decode::test_completions(2);
        tx.send(result(4, 4)).unwrap();
        tx.send(result(2, 4)).unwrap();

        let mut published = Vec::new();
        p.select_with(Load, |d| published.push(d.id));
        assert!(published.is_empty(), "load must not publish decoded images");
        assert_eq!(
            p.ready.len(),
            3,
            "load retains all live completions and prunes the retired one"
        );
        assert!(!p.ready.contains(&1), "retired readiness is pruned");
        assert_eq!(
            image_decode::outstanding_count(),
            0,
            "load collects worker completions"
        );

        p.select_with(Flush, |d| published.push(d.id));
        assert_eq!(
            published,
            [3, 4, 2],
            "flush follows the live completion arrival order"
        );
        assert!(p.requests.is_empty());
        assert!(p.ready.is_empty());
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
                p.select(Pump).len(),
                2,
                "two 4-byte images fit the 10-byte frame budget"
            );
        });
        crate::stone_reset_frame_stats();
        set_budget(10);
        PUBLICATIONS.with(|p| {
            assert!(
                p.borrow_mut().select(Pump).is_empty(),
                "neither statistics reset nor setting a budget refunds uploads"
            )
        });
        stone_begin_image_upload_frame();
        PUBLICATIONS.with(|p| {
            let mut p = p.borrow_mut();
            assert_eq!(p.select(Pump).len(), 2);
            assert!(p.requests.is_empty());
        });
        PUBLICATIONS.with(|p| *p.borrow_mut() = Publications::default());
    }

    fn classed(classes: &[RequestClass]) -> Publications {
        let mut p = Publications {
            budget: 10,
            ..Publications::default()
        };
        for (i, class) in classes.iter().enumerate() {
            let id = i as u64 + 1;
            p.submit(id, 4, 1, *class);
        }
        p
    }

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
    fn blocking_pump_drains_both_request_classes_in_readiness_order() {
        if isolated("blocking_pump_drains_both_request_classes_in_readiness_order") {
            return;
        }
        let mut p = classed(&[
            RequestClass::Deferred,
            RequestClass::Required,
            RequestClass::Deferred,
        ]);
        p.budget = 0;
        p.receive(result(2, 4));
        let tx = image_decode::test_completions(2);
        let worker = std::thread::spawn(move || {
            tx.send(result(1, 4)).unwrap();
            tx.send(result(3, 4)).unwrap();
        });
        let published = p.select(Pump);
        worker.join().unwrap();
        assert_eq!(ids(&published), [2, 1, 3]);
        assert!(p.requests.is_empty());
        assert_eq!(image_decode::outstanding_count(), 0);
    }

    #[test]
    fn require_turns_a_deferred_request_into_a_flush_wait() {
        if isolated("require_turns_a_deferred_request_into_a_flush_wait") {
            return;
        }
        let mut p = classed(&[RequestClass::Deferred, RequestClass::Deferred]);
        p.receive(result(1, 4));
        let tx = image_decode::test_completions(1);
        let (signal, worker) = release_on_signal(tx, 2);
        assert!(
            p.select(Flush).is_empty(),
            "a flush does not wait for Deferred work"
        );
        assert!(p.require(1), "a received Deferred request is promoted");
        assert!(p.require(2), "an outstanding Deferred request is promoted");
        assert!(!p.require(1), "a Required request has nothing to promote");
        let _ = signal.send(());
        assert_eq!(ids(&p.select(Flush)), [1, 2]);
        assert!(worker.join().unwrap());
        assert!(p.ready(1));
        assert!(!p.require(1), "a published request has nothing to promote");
    }

    #[test]
    fn pumps_share_one_frame_budget_with_progress_for_an_oversized_image() {
        if isolated("pumps_share_one_frame_budget_with_progress_for_an_oversized_image") {
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
        let sizes = |selected: Vec<Decoded>| {
            selected
                .iter()
                .map(|d| d.pixels.as_ref().unwrap().bytes().len())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            sizes(p.select(Pump)),
            [12],
            "one item over the budget still publishes alone"
        );
        assert!(
            p.select(Pump).is_empty(),
            "a second pump in the same frame cannot renew the spent allowance"
        );
        p.begin_frame();
        assert_eq!(sizes(p.select(Pump)), [4]);
        p.receive(result(3, 4));
        assert_eq!(sizes(p.select(Pump)), [4]);
        p.receive(result(4, 4));
        assert!(
            p.select(Pump).is_empty(),
            "an image larger than the remainder waits for the next frame"
        );
        p.begin_frame();
        assert_eq!(sizes(p.select(Pump)), [4]);
        p.submit(5, 4, 1, RequestClass::Required);
        p.receive(result(5, 12));
        assert_eq!(
            sizes(p.select(Flush)),
            [12],
            "Required flushes publish even when they exceed the frame cap"
        );
        assert!(p.requests.is_empty());
    }

    #[test]
    fn destroying_a_handle_retires_its_request_so_no_flush_waits_for_it() {
        if isolated("destroying_a_handle_retires_its_request_so_no_flush_waits_for_it") {
            return;
        }
        PUBLICATIONS
            .with(|p| *p.borrow_mut() = classed(&[RequestClass::Required, RequestClass::Deferred]));
        let tx = image_decode::test_completions(2);
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
                p.borrow().required_unreceived() == 0,
                "a destroyed Required request must not hold a flush"
            )
        });
        PUBLICATIONS.with(|p| p.borrow_mut().select(Flush));
        tx.send(result(1, 4)).unwrap();
        PUBLICATIONS.with(|p| {
            let mut p = p.borrow_mut();
            assert!(p.select(Pump).is_empty());
            assert_eq!(p.pending_bytes(), 16);
        });
    }
}
