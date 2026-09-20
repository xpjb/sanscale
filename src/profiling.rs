//! Opt-in work counters for regression investigations (`perf-counters` feature).
//!
//! Counters aggregate **all services on the calling thread**, not a particular
//! `TextService`. They do not collect driver/background-thread or GPU execution
//! work. Reset around the operation under investigation; snapshot afterwards.
//! Probes compile out, including their arguments, without this feature.
//!
//! Instrumented timing is not production timing. Run the pathological benchmark
//! twice: without this feature for latency, with it for work/allocation counts.
//! Counts describe actual work, not estimates based on elapsed time. In particular
//! `flow_glyph_tests` counts the current token/filter algorithm's inspected glyphs,
//! whereas `shape_runs` excludes the separately counted fallback probe shapings.

use std::cell::Cell;

macro_rules! counters {
    ($($(#[$doc:meta])* $field:ident),* $(,)?) => {
        /// Cumulative counts since the last reset on this thread.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct WorkCounters { $($(#[$doc])* pub $field: u64,)* }
        impl WorkCounters {
            /// Named values, without allocating. Useful for reports.
            pub fn values(self) -> impl Iterator<Item = (&'static str, u64)> {
                [$( (stringify!($field), self.$field), )*].into_iter()
            }
        }
    };
}

counters! {
    /// Calls to identity-keyed block shaping.
    block_requests,
    /// Blocks returned without paragraph lookup or assembly.
    block_hits,
    /// Individual paragraph cache lookups.
    paragraph_requests,
    /// Paragraph lookups which reused a flowed result.
    paragraph_hits,
    /// Calls into the consumer's paragraph source.
    source_reads,
    /// UTF-8 bytes returned by the consumer's source.
    source_bytes,
    /// Calls to the main paragraph shaper.
    shape_calls,
    /// UTF-8 bytes submitted to the main paragraph shaper.
    shape_bytes,
    /// Validated font spans supplied on paragraph cache misses.
    font_spans,
    /// Immutable paint snapshots registered.
    paint_registrations,
    /// Paint ranges copied into the service pool.
    paint_span_copies,
    /// Live paint snapshots explicitly released (including clear).
    paint_releases,
    /// Visible monochrome glyph foreground lookups.
    paint_lookups,
    /// Binary paint-range searches after leaving the cached span/gap.
    paint_searches,
    /// Actual rustybuzz calls for itemized font runs.
    shape_runs,
    /// Additional rustybuzz calls used to resolve/probe fallback sequences.
    fallback_shape_calls,
    /// Glyphs returned by the main run shapings, including whitespace.
    shaped_glyphs,
    /// .notdef glyphs in those main shapings; a benchmark must not mistake tofu for coverage.
    missing_glyphs,
    /// Font cmap coverage queries through Font::has_glyph.
    coverage_queries,
    /// Outline/band-cache hits.
    glyph_hits,
    /// Outline/band-cache misses (not all glyphs have an outline).
    glyph_misses,
    /// Glyphs whose outlines/bands were inserted into the cache.
    glyph_inserts,
    /// Calls to paragraph flow.
    flow_calls,
    /// Input glyph count across flow calls.
    flow_glyphs,
    /// Whitespace/non-whitespace tokens examined by flow.
    flow_tokens,
    /// Glyph membership tests in the current per-token filter, counted arithmetically.
    flow_glyph_tests,
    /// Visual lines produced by flow.
    flow_lines,
    /// Composed blocks assembled from paragraph results.
    assemblies,
    /// Lines copied during assembly.
    assembled_lines,
    /// Glyphs copied during assembly.
    assembled_glyphs,
    /// Carets copied during assembly.
    assembled_carets,
    /// Paragraph entries removed by capacity eviction.
    paragraph_evictions,
    /// Blocks removed by capacity eviction.
    block_evictions,
    /// Calls to prepare, including calls made by transient draw helpers.
    prepares,
    /// Draw items submitted to prepare.
    prepared_items,
    /// CPU geometry-cache hits.
    geometry_hits,
    /// CPU geometry rebuilds.
    geometry_builds,
    /// Layout lines considered by the visibility walk.
    visited_lines,
    /// Entire lines rejected by that walk.
    culled_lines,
    /// Glyphs visited after line culling, before glyph culling.
    visited_glyphs,
    /// Monochrome glyph quads emitted on geometry rebuilds.
    text_quads,
    /// Color glyph quads emitted on geometry rebuilds.
    emoji_quads,
    /// Clip-distinct segments prepared.
    prepared_segments,
    /// GPU vertex-buffer allocations for batches.
    batch_buffers,
    /// Vertex bytes written to GPU batch buffers, excluding allocation padding.
    vertex_upload_bytes,
    /// Text-pipeline draw commands actually recorded.
    text_draw_calls,
    /// Emoji-pipeline draw commands actually recorded.
    emoji_draw_calls,
    /// Curve/band texture pairs created, including initial empty resources.
    text_atlas_allocations,
    /// Bytes passed to curve/band queue texture writes (not driver transfer padding).
    text_atlas_upload_bytes,
    /// Emoji GPU textures created, including initial empty resources.
    emoji_atlas_allocations,
    /// Bytes passed to emoji queue texture writes.
    emoji_atlas_upload_bytes,
    /// Matrix-uniform bytes written by the service.
    uniform_upload_bytes,
    /// Emoji raster-cache hits, including cached unrenderable glyphs.
    emoji_hits,
    /// Emoji raster-cache misses and attempted rasterizations.
    emoji_rasterizations,
    /// Emoji atlas cells evicted.
    emoji_evictions,
    /// Emoji requests dropped because the working set exceeded atlas capacity.
    emoji_drops,
}

thread_local! {
    static COUNTERS: Cell<WorkCounters> = Cell::new(WorkCounters::default());
}

pub(crate) fn record(f: impl FnOnce(&mut WorkCounters)) {
    COUNTERS.with(|cell| {
        let mut counters = cell.get();
        f(&mut counters);
        cell.set(counters);
    });
}

/// Snapshot this thread's work. Does not reset or allocate.
pub fn work_counters() -> WorkCounters {
    COUNTERS.with(Cell::get)
}

/// Reset this thread only. Does not affect any cache or another thread's counters.
pub fn reset_work_counters() {
    COUNTERS.with(|cell| cell.set(WorkCounters::default()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_reset_and_are_thread_local() {
        reset_work_counters();
        crate::work::count!(shape_calls, 3);
        std::thread::spawn(|| {
            assert_eq!(work_counters().shape_calls, 0);
            crate::work::count!(shape_calls, 8);
            assert_eq!(work_counters().shape_calls, 8);
        })
        .join()
        .unwrap();
        assert_eq!(work_counters().shape_calls, 3);
        reset_work_counters();
        assert_eq!(work_counters(), WorkCounters::default());
    }
}
