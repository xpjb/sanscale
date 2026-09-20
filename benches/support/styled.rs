//! Feature-specific controls, added without replacing any legacy workload.
use super::{Fonts, Suite, Times, gpu::Gpu, measure, ns, style};
use crate::Doc;
use sanscale::{
    BlockKey, Color, Draw, FontSpan, PaintSpan, ParagraphKey, ParagraphSource, Rect, Vec2,
};
use serde_json::json;
use std::{borrow::Cow, hint::black_box, time::Instant};

struct Source {
    doc: Doc,
    spans: Vec<FontSpan>,
}
impl ParagraphSource for Source {
    fn paragraph_text(&self, index: usize, key: ParagraphKey) -> Option<Cow<'_, str>> {
        self.doc.paragraph_text(index, key)
    }
    fn paragraph_fonts(&self, _: usize, _: ParagraphKey) -> Cow<'_, [FontSpan]> {
        Cow::Borrowed(&self.spans)
    }
}
pub fn cpu(suite: &mut Suite<'_>, fonts: &Fonts) {
    let n = suite.options.sizes().long;
    for action in [
        "empty",
        "whole",
        "dense_equivalent",
        "dense_alternating",
        "font_edit_middle",
    ] {
        let name = format!("cpu.spans.{action}");
        if !suite.should_run(&name) {
            continue;
        }
        let (mut text, c) = fonts.install();
        let st = style(c.normal, Some(80.));
        let mut source = Source {
            doc: Doc::one("a".repeat(n)),
            spans: Vec::new(),
        };
        // Warm both actual outlines, not the measured paragraph layouts.
        text.shape_transient("a", &st).unwrap();
        text.shape_transient("a", &style(c.italic, Some(80.)))
            .unwrap();
        source.spans = match action {
            "whole" => vec![FontSpan {
                range: 0..n,
                chain: c.italic,
            }],
            "dense_equivalent" => (0..n)
                .map(|i| FontSpan {
                    range: i..i + 1,
                    chain: c.duplicate,
                })
                .collect(),
            "dense_alternating" => (0..n)
                .map(|i| FontSpan {
                    range: i..i + 1,
                    chain: if i % 2 == 0 { c.normal } else { c.italic },
                })
                .collect(),
            _ => Vec::new(),
        };
        suite.case(
            &name,
            json!({"bytes":n,"span_mode":action,"glyph_cache":"warm","paragraph_layout":"miss"}),
            1,
            &[("shape_calls", 1)],
            |i| {
                if action == "font_edit_middle" {
                    source.spans = if i % 2 == 0 {
                        vec![FontSpan {
                            range: n / 2..n / 2 + 3,
                            chain: c.italic,
                        }]
                    } else {
                        Vec::new()
                    };
                }
                source.doc.bump(0);
                measure(|| {
                    black_box(
                        text.shape(BlockKey(700), &st, &source.doc.keys, &source)
                            .unwrap(),
                    );
                    Times::default()
                })
            },
        );
    }
    for count in [1, suite.options.sizes().glyphs] {
        suite.case(
            &format!("cpu.spans.paint_register_drop.{count}"),
            json!({"spans":count,"interning":false}),
            1,
            &[("shape_calls", 0), ("flow_calls", 0)],
            {
                let (mut text, _) = fonts.install();
                let spans = (0..count)
                    .map(|i| PaintSpan {
                        range: i..i + 1,
                        color: Color([0.4, 0.6, 0.8, 1.]),
                    })
                    .collect::<Vec<_>>();
                move |_| {
                    measure(|| {
                        let h = text.register_paint(&spans).unwrap();
                        text.drop_paint(black_box(h));
                        Times::default()
                    })
                }
            },
        );
    }
}
pub fn gpu(suite: &mut Suite<'_>, fonts: &Fonts, gpu: &Gpu) {
    let n = suite.options.sizes().glyphs;
    for action in [
        "paint_sparse_recolor",
        "paint_dense_recolor",
        "paint_retained",
        "paint_released_retained",
    ] {
        let name = format!("gpu.spans.{action}");
        if !suite.should_run(&name) {
            continue;
        }
        let (mut text, c) = fonts.install();
        let doc = Doc::one("a".repeat(n));
        let st = style(c.normal, Some(80.));
        let block = text.shape(BlockKey(700), &st, &doc.keys, &doc).unwrap();
        gpu.attach(&mut text);
        let step = if action == "paint_sparse_recolor" {
            64
        } else {
            1
        };
        let spans = (0..n)
            .step_by(step)
            .map(|i| PaintSpan {
                range: i..(i + 1).min(n),
                color: Color([0.3, 0.7, 0.5, 1.]),
            })
            .collect::<Vec<_>>();
        let a = text.register_paint(&spans).unwrap();
        let other = spans
            .iter()
            .map(|s| PaintSpan {
                range: s.range.clone(),
                color: Color([0.7, 0.3, 0.5, 1.]),
            })
            .collect::<Vec<_>>();
        let b = text.register_paint(&other).unwrap();
        let mut draw = Draw {
            block,
            at: Vec2::new(0., 0.),
            size: 12.,
            color: Color([0.8, 0.8, 0.8, 1.]),
            clip: Some(Rect::new(0., 0., 1280., 768.)),
            paint: Some(a),
        };
        let retained = text.prepare(&gpu.device, &gpu.queue, &[draw]);
        gpu.drain();
        let is_retained = action.ends_with("retained");
        if action == "paint_released_retained" {
            text.drop_paint(a);
            assert!(text.batch_live(&retained));
            #[cfg(feature = "perf-counters")]
            sanscale::profiling::reset_work_counters();
            // A stale paint must not revive previously cached CPU geometry.
            let empty = text.prepare(&gpu.device, &gpu.queue, &[draw]);
            black_box(empty);
            gpu.drain();
            #[cfg(feature = "perf-counters")]
            assert_eq!(sanscale::profiling::work_counters().vertex_upload_bytes, 0);
        }
        let mut budgets = vec![("shape_calls", 0), ("flow_calls", 0), ("glyph_inserts", 0)];
        if is_retained {
            budgets.extend([
                ("prepares", 0),
                ("vertex_upload_bytes", 0),
                ("paint_lookups", 0),
            ]);
        }
        suite.case(
            &name,
            json!({"bytes":n,"spans":spans.len(),"paint":"immutable snapshots","clip":"viewport"}),
            1,
            &budgets,
            |i| {
                draw.paint = Some(if i % 2 == 0 { b } else { a });
                measure(|| {
                    if is_retained {
                        assert!(text.batch_live(&retained));
                        gpu.draw_batch(&text, &retained)
                    } else {
                        let start = Instant::now();
                        let batch = text.prepare(&gpu.device, &gpu.queue, &[draw]);
                        let prepare_ns = ns(start);
                        let mut times = gpu.draw_batch(&text, &batch);
                        times.prepare_ns = Some(prepare_ns);
                        times
                    }
                })
            },
        );
    }
}
