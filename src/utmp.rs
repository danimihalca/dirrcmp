//! Unique suffixes for test scratch directories (tests run in parallel).
use std::sync::atomic::{AtomicUsize, Ordering};

static N: AtomicUsize = AtomicUsize::new(0);

pub fn next() -> usize {
    N.fetch_add(1, Ordering::SeqCst)
}
