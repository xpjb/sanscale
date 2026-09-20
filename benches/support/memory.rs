//! Only the instrumented benchmark installs this allocator. Counts requested
//! Rust heap bytes, not resident memory, native driver allocations, or GPU memory.
//! All process threads are included; driver background Rust work can add noise.

#[cfg(feature = "perf-counters")]
mod enabled {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static LIVE: AtomicU64 = AtomicU64::new(0);
    static BASE: AtomicU64 = AtomicU64::new(0);
    static PEAK: AtomicU64 = AtomicU64::new(0);
    static ALLOCS: AtomicU64 = AtomicU64::new(0);
    static FREES: AtomicU64 = AtomicU64::new(0);
    static REQUESTED: AtomicU64 = AtomicU64::new(0);
    struct Counting;
    #[global_allocator]
    static ALLOCATOR: Counting = Counting;
    fn add(n: usize) {
        let live = LIVE.fetch_add(n as u64, Relaxed) + n as u64;
        if ACTIVE.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            REQUESTED.fetch_add(n as u64, Relaxed);
            PEAK.fetch_max(live, Relaxed);
        }
    }
    fn remove(n: usize) {
        LIVE.fetch_sub(n as u64, Relaxed);
        if ACTIVE.load(Relaxed) {
            FREES.fetch_add(1, Relaxed);
        }
    }
    // SAFETY: every operation delegates to System with the original pointer and
    // layout. Accounting only runs after successful allocations; failed realloc
    // leaves the old allocation live. No allocation is performed by the counters.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(l) };
            if !p.is_null() {
                add(l.size());
            }
            p
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            let p = unsafe { System.alloc_zeroed(l) };
            if !p.is_null() {
                add(l.size());
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            remove(l.size());
            unsafe {
                System.dealloc(p, l);
            }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
            let new = unsafe { System.realloc(p, l, size) };
            if !new.is_null() {
                remove(l.size());
                add(size);
            }
            new
        }
    }
    pub fn start() {
        assert!(!ACTIVE.load(Relaxed));
        ALLOCS.store(0, Relaxed);
        FREES.store(0, Relaxed);
        REQUESTED.store(0, Relaxed);
        let live = LIVE.load(Relaxed);
        BASE.store(live, Relaxed);
        PEAK.store(live, Relaxed);
        ACTIVE.store(true, Relaxed);
    }
    pub fn stop() -> serde_json::Value {
        ACTIVE.store(false, Relaxed);
        let base = BASE.load(Relaxed);
        let allocs = ALLOCS.load(Relaxed);
        let frees = FREES.load(Relaxed);
        let requested = REQUESTED.load(Relaxed);
        let delta = LIVE.load(Relaxed) as i128 - base as i128;
        let peak = PEAK.load(Relaxed).saturating_sub(base);
        // Snapshot everything before the JSON object itself allocates.
        serde_json::json!({"alloc_calls":allocs,"free_calls":frees,
            "requested_bytes":requested,"live_bytes_delta":delta,"peak_extra_live_bytes":peak})
    }
}
#[cfg(feature = "perf-counters")]
pub use enabled::{start, stop};
#[cfg(not(feature = "perf-counters"))]
pub fn start() {}
#[cfg(not(feature = "perf-counters"))]
pub fn stop() -> serde_json::Value {
    serde_json::Value::Null
}
