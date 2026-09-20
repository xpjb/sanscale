//! Compile-time-elided instrumentation. No counter expressions run in normal builds.

macro_rules! count {
    ($field:ident, $amount:expr) => {{
        #[cfg(feature = "perf-counters")]
        crate::profiling::record(|work| work.$field += ($amount) as u64);
    }};
}
pub(crate) use count;

#[cfg(all(test, not(feature = "perf-counters")))]
mod tests {
    #[test]
    fn disabled_probes_do_not_evaluate_their_arguments() {
        let touched = std::cell::Cell::new(false);
        super::count!(shape_calls, {
            touched.set(true);
            1
        });
        assert!(!touched.get());
    }
}
