//! Public-API regressions for the pre-0.1 review. Controls compare actual layouts
//! and readback pixels, not opaque handle encodings or cache bookkeeping alone.
mod support;
use support::{Gpu, W, H};
use sanscale::{Align, BlockKey, Color, Draw, FontHandle, ParagraphKey, Paragraphs, Style, TextService, Vec2, read_font_file};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const LATIN: &str = "/usr/share/fonts/TTF/DejaVuSans.ttf";
const MONO: &str = "/usr/share/fonts/TTF/DejaVuSansMono.ttf";
const EMOJI: &str = "/usr/share/fonts/noto/NotoColorEmoji.ttf";

fn font(t: &mut TextService, path: &str) -> FontHandle {
    let alternative = path.replace("/TTF/", "/truetype/dejavu/").replace("/fonts/noto/", "/fonts/truetype/noto/");
    let data = read_font_file(path).or_else(|_| read_font_file(&alternative))
        .expect("install DejaVu Sans/Mono and Noto Color Emoji, or adjust the font paths above");
    t.map_font(data, 0).unwrap()
}
fn style(t: &mut TextService, path: &str) -> Style {
    let f = font(t, path);
    Style { chain: t.register_chain(&[f]).unwrap(), wrap_em: None, align: Align::Left, line_spacing: 1.0 }
}
fn draw(t: &mut TextService, s: &Style, content: &str, size: f32) -> Draw {
    Draw { block: t.shape_transient(content, s).unwrap(), at: Vec2::new(8., 8.), size, color: Color([1.; 4]), ..Default::default() }
}
fn ink(pixels: &[u8]) -> usize { pixels.chunks_exact(4).filter(|p| p[..3] != [0, 0, 0]).count() }
fn diff(a: &[u8], b: &[u8]) -> usize { a.chunks_exact(4).zip(b.chunks_exact(4)).filter(|(a, b)| a != b).count() }


#[test]
fn reanchoring_uses_the_same_hard_break_rule_as_placement() {
    let mut t = TextService::new();
    let wide = style(&mut t, LATIN);
    let narrow = Style { wrap_em: Some(0.7), ..wide };
    let old = t.shape_transient("ab\ncd", &narrow).unwrap();
    let caret = t.measure(old).caret_at(2);
    assert_eq!(caret.line_index, 1, "fixture must wrap before the hard break");
    let new = t.shape_transient("ab\ncd", &wide).unwrap();
    let layout = t.measure(new);
    let placed = layout.clamp_caret(caret);
    assert_eq!(placed, layout.caret_at(2));
    assert_eq!(placed.line_index, 0);
    assert_eq!(layout.caret_rect(placed).y_em, 0.);
    assert_eq!(layout.caret_after_edit(2), placed);
}

#[test]
fn chain_capacity_fails_without_aliasing_a_live_font_choice() {
    let mut t = TextService::new();
    let sans = style(&mut t, LATIN);
    let mono_font = font(&mut t, MONO);
    let mono = Style { chain: t.register_chain(&[mono_font]).unwrap(), ..sans };
    let a = t.shape_transient("WWWiii", &sans).unwrap();
    let b = t.shape_transient("WWWiii", &mono).unwrap();
    let expected = (t.measure(a).width_em(), t.measure(b).width_em());
    assert_ne!(expected.0, expected.1);
    for _ in 2..65_535 { t.register_chain(&[mono_font]).unwrap(); }
    assert_eq!(t.register_chain(&[mono_font]), Err(sanscale::FontError::PoolFull));
    let a = t.shape_transient("WWWiii", &sans).unwrap();
    let b = t.shape_transient("WWWiii", &mono).unwrap();
    assert_eq!((t.measure(a).width_em(), t.measure(b).width_em()), expected);
    // A released slot remains usable at capacity, without reviving its handle.
    t.drop_chain(mono.chain);
    let replacement = Style { chain: t.register_chain(&[mono_font]).unwrap(), ..mono };
    assert!(t.shape_transient("WWWiii", &mono).is_none());
    let b = t.shape_transient("WWWiii", &replacement).unwrap();
    assert_eq!(t.measure(b).width_em(), expected.1);
}

#[test]
fn stale_chain_release_cannot_destroy_a_replacement_even_after_clear() {
    let mut t = TextService::new();
    let old = style(&mut t, LATIN);
    t.drop_chain(old.chain);
    let current = style(&mut t, MONO);
    let h = t.shape_transient("WWWiii", &current).unwrap();
    let expected = t.measure(h).width_em();
    t.drop_chain(old.chain);
    assert!(t.shape_transient("WWWiii", &old).is_none());
    assert_eq!(t.measure(h).width_em(), expected);
    t.clear();
    let fresh = style(&mut t, MONO);
    t.drop_chain(old.chain);
    t.drop_chain(current.chain);
    assert!(t.shape_transient("WWWiii", &current).is_none());
    let h = t.shape_transient("WWWiii", &fresh).unwrap();
    assert_eq!(t.measure(h).width_em(), expected);
}

#[test]
#[ignore = "headless wgpu adapter and DejaVu/Noto fonts"]
fn recorded_passes_own_their_transforms_for_text_and_emoji() {
    let g = Gpu::new();
    for (file, label) in [(LATIN, "Ag"), (EMOJI, "😀")] {
        let mut t = TextService::new(); let s = style(&mut t, file);
        // Setter order is no longer a silent no-op before GPU initialization.
        let a = TextService::pixel_ortho(W, H);
        t.set_transform(a); t.set_target(&g.d, FORMAT);
        let item = draw(&mut t, &s, label, 48.);
        let b = t.prepare(&g.d, &g.q, &[item]);
        let reference_a = g.render(&t, &[&b], FORMAT);
        let mut shifted = a; shifted[12] += 0.75;
        t.set_transform(shifted);
        let reference_b = g.render(&t, &[&b], FORMAT);
        assert!(ink(&reference_a) > 0 && ink(&reference_b) > 0 && reference_a != reference_b);
        t.set_transform(a);
        let (ca, out_a) = g.encode(&t, &[&b], FORMAT);
        t.set_transform(shifted);
        let (cb, out_b) = g.encode(&t, &[&b], FORMAT);
        drop(b); // Recorded resource ownership, not just the Rust batch's lifetime.
        g.q.submit([ca, cb]);
        assert_eq!(diff(&g.read(&out_a), &reference_a), 0);
        assert_eq!(diff(&g.read(&out_b), &reference_b), 0);
    }
}

#[test]
#[ignore = "headless wgpu adapter and DejaVu/Noto fonts"]
fn target_changes_preserve_atlases_matrix_and_retained_batches() {
    let g = Gpu::new();
    let mut t = TextService::new(); let latin = style(&mut t, LATIN); let emoji = style(&mut t, EMOJI);
    g.attach(&mut t, FORMAT);
    let a = draw(&mut t, &latin, "Ag", 48.);
    let mut b = draw(&mut t, &emoji, "😀", 48.); b.at.x += 80.;
    let batch = t.prepare(&g.d, &g.q, &[a, b]);
    let control = g.render(&t, &[&batch], FORMAT);
    assert!(ink(&control) > 0);
    #[cfg(feature = "perf-counters")]
    sanscale::profiling::reset_work_counters();
    for format in [wgpu::TextureFormat::Bgra8Unorm, FORMAT, wgpu::TextureFormat::Bgra8Unorm] {
        t.set_target(&g.d, format); // Do not reset the transform or prepare again.
        assert!(t.batch_live(&batch));
        let mut pixels = g.render(&t, &[&batch], format);
        if format == wgpu::TextureFormat::Bgra8Unorm {
            for p in pixels.chunks_exact_mut(4) { p.swap(0, 2); }
        }
        assert_eq!(diff(&pixels, &control), 0);
    }
    #[cfg(feature = "perf-counters")]
    {
        let w = sanscale::profiling::work_counters();
        assert_eq!((w.text_atlas_allocations, w.emoji_atlas_allocations, w.uniform_upload_bytes), (0, 0, 0));
    }
}

#[test]
#[ignore = "headless wgpu adapter and DejaVu/Noto fonts"]
fn batched_overlap_order_matches_sequential_draws() {
    let g = Gpu::new();
    let mut t = TextService::new(); let latin = style(&mut t, LATIN); let emoji = style(&mut t, EMOJI); g.attach(&mut t, FORMAT);
    let a = draw(&mut t, &emoji, "😀", 96.);
    let mut b = draw(&mut t, &latin, "MMMM", 96.); b.color = Color([1., 0., 0., 1.]);
    let ba = t.prepare(&g.d, &g.q, &[a]); let bb = t.prepare(&g.d, &g.q, &[b]);
    let forward = g.render(&t, &[&ba, &bb], FORMAT);
    let reverse = g.render(&t, &[&bb, &ba], FORMAT);
    assert!(forward != reverse, "control must actually overlap");
    for (items, expected) in [([a, b], &forward), ([b, a], &reverse)] {
        let batch = t.prepare(&g.d, &g.q, &items);
        assert_eq!(batch.segments().len(), 1, "equal clips still coalesce");
        assert_eq!(diff(&g.render(&t, &[&batch], FORMAT), expected), 0);
    }
    // The convenience path has exactly the same fields, including paint.
    let paint = t.register_paint(&[sanscale::PaintSpan { range: 0..4, color: Color([0., 1., 0., 1.]) }]).unwrap();
    let painted = Draw { paint: Some(paint), ..b };
    let painted_batch = t.prepare(&g.d, &g.q, &[painted]);
    let expected = g.render(&t, &[&painted_batch], FORMAT);
    assert!(expected != g.render(&t, &[&bb], FORMAT));
    let (commands, out) = g.encode_with(FORMAT, |pass| t.draw(&g.d, &g.q, pass, painted));
    g.q.submit([commands]);
    assert_eq!(diff(&g.read(&out), &expected), 0);
    t.drop_paint(paint);
    assert!(t.batch_live(&painted_batch));
    assert_eq!(diff(&g.render(&t, &[&painted_batch], FORMAT), &expected), 0);
    let stale_paint = t.prepare(&g.d, &g.q, &[painted]);
    assert!(!t.batch_live(&stale_paint));

    // Interleaving inside one block, not just between Draws. Zero line spacing
    // overlays two paragraphs; separate single-paragraph draws are the control.
    let latin_font = font(&mut t, LATIN); let emoji_font = font(&mut t, EMOJI);
    let mixed = Style { chain: t.register_chain(&[latin_font, emoji_font]).unwrap(), line_spacing: 0., ..latin };
    for (label, left, right) in [("😀\nMMMM", "😀", "MMMM"), ("MMMM\n😀", "MMMM", "😀")] {
        let mut combined = draw(&mut t, &mixed, label, 96.); combined.at.y = 120.;
        let mut a = draw(&mut t, &mixed, left, 96.); a.at.y = 120.;
        let mut b = draw(&mut t, &mixed, right, 96.); b.at.y = 120.;
        let ba = t.prepare(&g.d, &g.q, &[a]); let bb = t.prepare(&g.d, &g.q, &[b]);
        let expected = g.render(&t, &[&ba, &bb], FORMAT);
        let bc = t.prepare(&g.d, &g.q, &[combined]);
        assert!(ink(&expected) > 0);
        assert_eq!(diff(&g.render(&t, &[&bc], FORMAT), &expected), 0);
    }
}

#[test]
#[ignore = "headless wgpu adapter and DejaVu font"]
fn eviction_recovery_refreshes_handles_not_just_buffers() {
    let g = Gpu::new();
    let mut t = TextService::new(); let s = style(&mut t, LATIN); g.attach(&mut t, FORMAT);
    let old = draw(&mut t, &s, "keep", 48.);
    let b = t.prepare(&g.d, &g.q, &[old]);
    let control = g.render(&t, &[&b], FORMAT);
    assert!(ink(&control) > 0);
    let keys = [ParagraphKey { namespace: 4, slot: 0, generation: 0 }];
    for n in 0..131_072 { t.shape(BlockKey(n), &s, &keys, &Paragraphs(&["x"])).unwrap(); }
    assert!(!t.batch_live(&b));
    let incomplete = t.prepare(&g.d, &g.q, &[old]);
    assert!(!t.batch_live(&incomplete), "missing text must not become a live empty cache entry");
    let fresh = draw(&mut t, &s, "keep", 48.);
    let rebuilt = t.prepare(&g.d, &g.q, &[fresh]);
    assert!(t.batch_live(&rebuilt));
    assert_eq!(diff(&g.render(&t, &[&rebuilt], FORMAT), &control), 0);
    let empty = t.prepare(&g.d, &g.q, &[]);
    assert!(t.batch_live(&empty), "a genuinely empty request is complete");
}

#[test]
#[ignore = "headless wgpu adapter and DejaVu/Noto fonts"]
fn retained_emoji_refresh_only_when_the_requested_bucket_changes() {
    let g = Gpu::new();
    let mut t = TextService::new(); let s = style(&mut t, EMOJI); let latin = style(&mut t, LATIN); g.attach(&mut t, FORMAT);
    let item = draw(&mut t, &s, "😀", 32.);
    let text = draw(&mut t, &latin, "Ag", 32.);
    let b = t.prepare(&g.d, &g.q, &[item]);
    let mono = t.prepare(&g.d, &g.q, &[text]);
    assert!(ink(&g.render(&t, &[&b], FORMAT)) > 0);
    t.set_pixel_scale(0.9);
    assert!(t.batch_live(&b), "same 32px bucket");
    let mut zoom = TextService::pixel_ortho(W, H); zoom[0] *= 4.; zoom[5] *= 4.;
    t.set_transform(zoom); t.set_pixel_scale(4.);
    assert!(!t.batch_live(&b));
    assert!(t.batch_live(&mono), "monochrome retention is zoom invariant");
    let low_resolution = g.render(&t, &[&b], FORMAT);
    let refreshed = t.prepare(&g.d, &g.q, &[item]);
    let expected = g.render(&t, &[&refreshed], FORMAT);
    assert!(t.batch_live(&refreshed));
    assert!(low_resolution != expected && ink(&expected) > 0);
}

#[test]
#[ignore = "headless wgpu adapter and Noto Color Emoji"]
fn emoji_pressure_preserves_retained_and_unsubmitted_draws() {
    let g = Gpu::new();
    let mut t = TextService::new(); let s = style(&mut t, EMOJI); g.attach(&mut t, FORMAT);
    let first = draw(&mut t, &s, "😀", 256.);
    let retained = t.prepare(&g.d, &g.q, &[first]);
    let control = g.render(&t, &[&retained], FORMAT);
    assert!(ink(&control) > 0);
    let temporary = t.prepare(&g.d, &g.q, &[first]);
    let (pending, readback) = g.encode(&t, &[&temporary], FORMAT);
    drop(temporary);
    #[cfg(feature = "perf-counters")]
    sanscale::profiling::reset_work_counters();
    // More than the resident cache budget, including a single batch larger than
    // that budget. Every page remains valid, even if evicted during prepare.
    t.set_pixel_scale(32.); // Large rasters, small visible quads: every glyph is observable.
    let mut items = Vec::new();
    for cp in 0x1f300..0x1f900 {
        let c = char::from_u32(cp).unwrap();
        if t.diagnostics().covers(s.chain, c) {
            let mut item = draw(&mut t, &s, &c.to_string(), 8.);
            item.at = Vec2::new(8. + (items.len() % 16) as f32 * 14., 8. + (items.len() / 16) as f32 * 18.);
            items.push(item);
        }
        if items.len() == 256 { break; }
    }
    assert_eq!(items.len(), 256, "enough color glyphs to exceed the budget");
    let all = t.prepare(&g.d, &g.q, &items);
    assert!(t.batch_live(&all));
    assert!(t.batch_live(&retained), "eviction does not invalidate owned page snapshots");
    assert_eq!(t.diagnostics().dropped_glyphs(), 0);
    let (pages, bytes) = t.diagnostics().emoji_cache_usage();
    assert!(pages > 0 && bytes <= 64 * 1024 * 1024);
    #[cfg(feature = "perf-counters")]
    assert!(sanscale::profiling::work_counters().emoji_evictions > 0);
    // A large batch must actually render every ordered page, including pages
    // evicted while that very batch was being prepared.
    let individual: Vec<_> = items.iter().map(|item| t.prepare(&g.d, &g.q, &[*item])).collect();
    let ordered: Vec<_> = individual.iter().collect();
    let expected_all = g.render(&t, &ordered, FORMAT);
    assert!(ink(&expected_all) > 0);
    assert_eq!(diff(&g.render(&t, &[&all], FORMAT), &expected_all), 0);
    // Submit a command encoded before the cache churn, after its batch was dropped.
    g.q.submit([pending]);
    assert_eq!(diff(&g.read(&readback), &control), 0);
    assert_eq!(diff(&g.render(&t, &[&retained], FORMAT), &control), 0);
    // Churn between all buckets as well: a full large-glyph cache cannot starve
    // a different bucket, and returning to an evicted glyph must rerasterize it.
    for scale in [0.125, 0.25, 0.5, 1.] {
        t.set_pixel_scale(scale);
        let fresh = t.prepare(&g.d, &g.q, &[first]);
        assert!(ink(&g.render(&t, &[&fresh], FORMAT)) > 0);
        assert_eq!(t.diagnostics().dropped_glyphs(), 0);
    }
    let fresh = t.prepare(&g.d, &g.q, &[first]);
    assert_eq!(diff(&g.render(&t, &[&fresh], FORMAT), &control), 0);
    #[cfg(feature = "perf-counters")]
    {
        sanscale::profiling::reset_work_counters();
        let warm = t.prepare(&g.d, &g.q, &[first]);
        assert_eq!(diff(&g.render(&t, &[&warm], FORMAT), &control), 0);
        let w = sanscale::profiling::work_counters();
        assert_eq!((w.emoji_rasterizations, w.emoji_atlas_upload_bytes), (0, 0));
    }
}

#[test]
fn caret_motion_accepts_trait_objects_and_saturating_page_strides() {
    use sanscale::{Boundaries, CaretStop, Layout, LayoutLineSpec, LineMetrics, Motion};
    let layout = Layout::from_lines((0..3).map(|i| LayoutLineSpec {
        byte_range: i * 2..i * 2 + 1,
        metrics: LineMetrics { top_em: i as f32, height_em: 1., width_em: 1., ..Default::default() },
        carets: vec![CaretStop { byte_index: i * 2, x_em: 0. }, CaretStop { byte_index: i * 2 + 1, x_em: 1. }],
    }).collect());
    let classifier: &dyn Boundaries = &();
    let c = layout.caret_at(3);
    let mut goal = None;
    assert_eq!(layout.caret_move(c, Motion::PageUp(0), &mut goal, classifier), c);
    assert_eq!(layout.caret_move(c, Motion::PageDown(0), &mut goal, classifier), c);
    assert_eq!(layout.caret_move(c, Motion::PageUp(usize::MAX), &mut goal, classifier).line_index, 0);
    assert_eq!(layout.caret_move(c, Motion::PageDown(usize::MAX), &mut goal, classifier).line_index, 2);
    assert!(!layout.select_word_at(2, classifier).is_empty());
}

#[test]
#[ignore = "headless wgpu adapter and DejaVu/Noto fonts"]
fn ordered_pipeline_runs_do_not_cross_scissor_segments() {
    let g = Gpu::new();
    let mut t = TextService::new(); let latin = style(&mut t, LATIN); let emoji = style(&mut t, EMOJI); g.attach(&mut t, FORMAT);
    let mut a = draw(&mut t, &emoji, "😀", 96.);
    a.clip = Some(sanscale::Rect::new(8., 8., 70., 120.));
    let mut b = draw(&mut t, &latin, "MMMM", 96.);
    b.color = Color([1., 0., 0., 1.]); b.clip = Some(sanscale::Rect::new(42., 12., 120., 120.));
    let items = [a, b, a];
    let merged = t.prepare(&g.d, &g.q, &items);
    assert_eq!(merged.segments().len(), 3);
    let separate: Vec<_> = items.iter().map(|item| t.prepare(&g.d, &g.q, &[*item])).collect();
    let render = |batches: &[&sanscale::Batch]| {
        let (commands, out) = g.encode_with(FORMAT, |pass| {
            for batch in batches {
                for (i, segment) in batch.segments().iter().enumerate() {
                    let clip = segment.clip.unwrap();
                    pass.set_scissor_rect(clip.x as u32, clip.y as u32, clip.width as u32, clip.height as u32);
                    t.draw_segment(pass, batch, i);
                }
            }
        });
        g.q.submit([commands]); g.read(&out)
    };
    let expected = render(&separate.iter().collect::<Vec<_>>());
    assert!(ink(&expected) > 0);
    assert_eq!(diff(&render(&[&merged]), &expected), 0);
}
