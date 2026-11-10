//! Counters split across threads, so that counting adds no shared write to a
//! hot path: each thread counts in the stripe it was dealt, on a cache line of
//! its own, and a reader folds every stripe.

const STRIPES: usize = 16;

#[derive(Default)]
#[repr(align(128))]
struct Stripe<T>(T);

pub(crate) struct Striped<T>(Box<[Stripe<T>]>);

impl<T: Default> Default for Striped<T> {
    fn default() -> Self {
        Self((0..STRIPES).map(|_| Stripe::default()).collect())
    }
}

impl<T> Striped<T> {
    /// The calling thread's stripe.
    pub(crate) fn local(&self) -> &T {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        thread_local! {
            static STRIPE: usize =
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % STRIPES;
        }
        &self.0[STRIPE.with(|stripe| *stripe)].0
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter().map(|stripe| &stripe.0)
    }
}
