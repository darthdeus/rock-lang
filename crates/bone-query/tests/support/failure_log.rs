use std::cell::RefCell;
use std::sync::Once;

thread_local! {
    static RECORDS: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if record.level() <= log::Level::Warn {
            RECORDS.with(|records| {
                if let Some(records) = records.borrow_mut().as_mut() {
                    records.push(format!("{} {}", record.target(), record.args()));
                }
            });
        }
    }

    fn flush(&self) {}
}

pub struct Recording;

impl Recording {
    pub fn start() -> Self {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            log::set_logger(&Capture).unwrap();
            log::set_max_level(log::LevelFilter::Trace);
        });
        RECORDS.with(|records| {
            assert!(records.borrow().is_none());
            *records.borrow_mut() = Some(Vec::new());
        });
        Self
    }

    pub fn take(&self) -> Vec<String> {
        RECORDS.with(|records| std::mem::take(records.borrow_mut().as_mut().unwrap()))
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        RECORDS.with(|records| *records.borrow_mut() = None);
    }
}
