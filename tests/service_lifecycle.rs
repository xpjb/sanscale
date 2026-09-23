//! Public-behavior checks: cached/interleaved requests must agree with isolated
//! layouts, and a reset service must render like a fresh one. No assertions about
//! hash values or the representation of keys, generations, or atlas revisions.
use sanscale::{
    Align, Batch, BlockKey, Color, Draw, FontData, Layout, ParagraphKey, Paragraphs, ShapedHandle,
    Style, TextService, Vec2, read_font_file,
};

fn latin_font() -> FontData {
    [
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "C:/Windows/Fonts/arial.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/Library/Fonts/Arial.ttf",
    ]
    .iter()
    .find_map(|path| read_font_file(path).ok())
    .expect("lifecycle tests require DejaVu Sans or Arial")
}

fn install(text: &mut TextService, font: &FontData) -> Style {
    let font = text.map_font(font.clone(), 0).unwrap();
    Style {
        chain: text.register_chain(&[font]).expect("font chain capacity"),
        wrap_em: None,
        align: Align::Left,
        line_spacing: 1.,
    }
}

fn snapshot(layout: &Layout) -> Vec<u64> {
    let mut out = vec![layout.len_bytes() as u64, layout.line_count() as u64];
    out.extend([layout.width_em(), layout.height_em()].map(|v| u64::from(v.to_bits())));
    for i in 0..layout.line_count() {
        let range = layout.line_range(i).unwrap();
        let line = layout.line(i).unwrap();
        out.extend([range.start as u64, range.end as u64]);
        out.extend(
            [line.top_em, line.baseline_em, line.height_em, line.width_em]
                .map(|v| u64::from(v.to_bits())),
        );
    }
    // Also observe alignment/hit-test coordinate space, not just line widths.
    for byte in 0..=layout.len_bytes() {
        let caret = layout.caret_rect(layout.caret_at(byte));
        out.extend([caret.x_em, caret.y_em, caret.height_em].map(|v| u64::from(v.to_bits())));
    }
    out
}

#[test]
fn cleared_handles_stay_empty_after_repeated_refills() {
    let font = latin_font();
    let mut text = TextService::new();
    let mut old = Vec::new();
    for _ in 0..3 {
        text.clear();
        text.clear(); // Re-clearing must not duplicate free slots.
        assert_eq!(text.diagnostics().cache_occupancy(), (0, 0));
        let style = install(&mut text, &font);
        // Leave an already-freed slot for the next clear, alongside live ones.
        let disposable = install(&mut text, &font);
        let dead = text.shape_transient("discarded", &disposable).unwrap();
        text.drop_chain(disposable.chain);
        old.push(dead);
        let current: Vec<_> = ["A", "BBBB", "third line\nlast line"]
            .into_iter()
            .map(|s| (text.shape_transient(s, &style).unwrap(), s.len()))
            .collect();
        for &(handle, len) in &current {
            assert_eq!(text.measure(handle).len_bytes(), len);
        }
        for &handle in &old {
            assert_eq!(text.measure(handle).line_count(), 0);
            assert_eq!(text.measure(handle).len_bytes(), 0);
        }
        old.extend(current.into_iter().map(|(h, _)| h));
    }
}

#[test]
fn transient_styles_match_isolated_named_layouts_without_changing_each_other() {
    let font = latin_font();
    let mut text = TextService::new();
    let base = install(&mut text, &font);
    let content = "alpha beta gamma delta\ncaf\u{e9} office";
    let lines: Vec<_> = content.split('\n').collect();
    let styles = [
        base,
        Style {
            wrap_em: Some(4.),
            ..base
        },
        Style {
            wrap_em: Some(30.),
            align: Align::Center,
            ..base
        },
        Style {
            wrap_em: Some(8.),
            align: Align::Right,
            line_spacing: 1.8,
            ..base
        },
    ];
    let mut retained = Vec::new();
    for style in styles {
        // Oracle: ordinary named shaping on an independent service. It has no
        // transient identities and no earlier requests that could be clobbered.
        let mut fresh = TextService::new();
        let chain = install(&mut fresh, &font).chain;
        let keys: Vec<_> = (0..lines.len())
            .map(|slot| ParagraphKey {
                namespace: 0,
                slot: slot as u32,
                generation: 0,
            })
            .collect();
        let expected = fresh
            .shape(
                BlockKey(0),
                &Style { chain, ..style },
                &keys,
                &Paragraphs(&lines),
            )
            .unwrap();
        let expected = snapshot(fresh.measure(expected));
        let actual = text.shape_transient(content, &style).unwrap();
        retained.push((actual, expected, style));
        for (h, expected, _) in &retained {
            assert_eq!(&snapshot(text.measure(*h)), expected);
        }
    }
    assert_ne!(
        retained[0].1, retained[1].1,
        "fixture must actually wrap differently"
    );
    for (h, expected, style) in retained {
        assert_eq!(text.shape_transient(content, &style), Some(h));
        assert_eq!(snapshot(text.measure(h)), expected);
    }
}

#[test]
fn independent_documents_can_reuse_local_slots_alongside_transient_labels() {
    let font = latin_font();
    let mut text = TextService::new();
    let style = install(&mut text, &font);
    let mut documents = Vec::new();
    // All namespace values belong to the consumer, including the upper boundary.
    for (namespace, content) in [(0, "first"), (1, "second document"), (u64::MAX, "third")] {
        let key = ParagraphKey {
            namespace,
            slot: 0,
            generation: 0,
        };
        let h = text
            .shape(BlockKey(namespace), &style, &[key], &Paragraphs(&[content]))
            .unwrap();
        documents.push((h, snapshot(text.measure(h))));
        text.shape_transient(
            content,
            &Style {
                wrap_em: Some(2.),
                ..style
            },
        )
        .unwrap();
    }
    for &(h, ref expected) in &documents {
        assert_eq!(&snapshot(text.measure(h)), expected);
    }
    let edited = ParagraphKey {
        namespace: 0,
        slot: 0,
        generation: 1,
    };
    let updated = text
        .shape(
            BlockKey(0),
            &style,
            &[edited],
            &Paragraphs(&["edited first document"]),
        )
        .unwrap();
    assert_eq!(
        updated, documents[0].0,
        "named blocks still update in place"
    );
    assert_eq!(
        text.measure(updated).len_bytes(),
        "edited first document".len()
    );
    for (h, expected) in documents.into_iter().skip(1) {
        assert_eq!(snapshot(text.measure(h)), expected);
    }
}

#[cfg(feature = "perf-counters")]
#[test]
fn transient_width_changes_reflow_but_do_not_reshape() {
    use sanscale::profiling::{reset_work_counters, work_counters};
    let font = latin_font();
    let mut text = TextService::new();
    let wide = install(&mut text, &font);
    let narrow = Style {
        wrap_em: Some(3.),
        ..wide
    };
    let content = "alpha beta gamma\ndelta epsilon";
    reset_work_counters();
    let a = text.shape_transient(content, &wide).unwrap();
    assert_eq!(
        work_counters().shape_calls,
        2,
        "control must do real shaping"
    );
    reset_work_counters();
    let b = text.shape_transient(content, &narrow).unwrap();
    let work = work_counters();
    assert_eq!(
        (work.shape_calls, work.shape_runs, work.glyph_inserts),
        (0, 0, 0)
    );
    assert_eq!(work.flow_calls, 2);
    assert!(text.measure(b).line_count() > text.measure(a).line_count());
    reset_work_counters();
    assert_eq!(text.shape_transient(content, &wide), Some(a));
    let work = work_counters();
    assert_eq!(
        (work.shape_calls, work.flow_calls, work.assemblies),
        (0, 0, 0)
    );
}

const WIDTH: u32 = 256; // RGBA rows are naturally COPY_BYTES_PER_ROW_ALIGNMENT aligned.
const HEIGHT: u32 = 96;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn device() -> (wgpu::Device, wgpu::Queue) {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("GPU lifecycle tests require a headless wgpu adapter");
        adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .unwrap()
    })
}

fn attach(text: &mut TextService, device: &wgpu::Device, _queue: &wgpu::Queue) {
    text.set_target(device, FORMAT);
    text.set_transform(TextService::pixel_ortho(WIDTH, HEIGHT));
}

fn render(
    text: &mut TextService,
    block: ShapedHandle,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) -> (Vec<u8>, Batch) {
    let extent = wgpu::Extent3d {
        width: WIDTH,
        height: HEIGHT,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lifecycle test target"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let batch = text.prepare(
        device,
        queue,
        &[Draw {
            block,
            at: Vec2::new(5., 5.),
            size: 48.,
            color: Color([1.; 4]),
            ..Default::default()
        }],
    );
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("lifecycle test readback"),
        size: u64::from(WIDTH * HEIGHT * 4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        text.draw_prepared(&mut pass, &batch);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(WIDTH * 4),
                rows_per_image: Some(HEIGHT),
            },
        },
        extent,
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv().unwrap().unwrap();
    (buffer.slice(..).get_mapped_range().unwrap().to_vec(), batch)
}

fn check_gpu_reset(font: FontData, cases: &[(&str, &str)], emoji: bool) {
    let (device, queue) = device();
    for &(before, after) in cases {
        let mut text = TextService::new();
        let style = install(&mut text, &font);
        attach(&mut text, &device, &queue);
        let old = text.shape_transient(before, &style).unwrap();
        let (before_pixels, old_batch) = render(&mut text, old, &device, &queue);
        assert!(text.batch_live(&old_batch));
        #[cfg(feature = "perf-counters")]
        sanscale::profiling::reset_work_counters();
        text.clear();
        assert!(!text.batch_live(&old_batch));
        let style = install(&mut text, &font);
        let new = text.shape_transient(after, &style).unwrap();
        // Deliberately do not reattach: clear must preserve target and transform.
        let (actual, new_batch) = render(&mut text, new, &device, &queue);
        assert!(text.batch_live(&new_batch));
        assert!(!text.batch_live(&old_batch));
        #[cfg(feature = "perf-counters")]
        {
            let work = sanscale::profiling::work_counters();
            assert_eq!(
                (work.text_atlas_allocations, work.emoji_atlas_allocations),
                (0, u64::from(emoji))
            );
            assert!(
                if emoji {
                    work.emoji_atlas_upload_bytes
                } else {
                    work.text_atlas_upload_bytes
                } > 0
            );
        }
        let _ = emoji; // Used by the work guards in instrumented builds.
        let mut fresh = TextService::new();
        let style = install(&mut fresh, &font);
        attach(&mut fresh, &device, &queue);
        let expected = fresh.shape_transient(after, &style).unwrap();
        let (expected, _) = render(&mut fresh, expected, &device, &queue);
        assert!(
            expected.chunks_exact(4).any(|p| p[..3] != [0, 0, 0]),
            "control must draw visible ink"
        );
        assert!(
            expected != before_pixels,
            "fixture must draw different glyphs, not identical tofu"
        );
        assert!(
            actual == expected,
            "reset/reload {after:?} differs from a fresh service (previously {before:?})"
        );
        #[cfg(feature = "perf-counters")]
        sanscale::profiling::reset_work_counters();
        let (repeated, _) = render(&mut text, new, &device, &queue);
        assert!(repeated == expected);
        #[cfg(feature = "perf-counters")]
        {
            let work = sanscale::profiling::work_counters();
            assert_eq!(
                (work.text_atlas_upload_bytes, work.emoji_atlas_upload_bytes),
                (0, 0)
            );
        }
        let (stale, _) = render(&mut text, old, &device, &queue);
        assert!(stale.chunks_exact(4).all(|p| p[..3] == [0, 0, 0]));
    }
}

#[test]
#[ignore = "requires a headless wgpu adapter and DejaVu Sans or Arial"]
fn gpu_clear_reload_matches_fresh_text_without_reallocating() {
    // Equal glyph counts catch revision reuse; unequal counts catch stale append
    // offsets. All fit within the original texture capacities.
    check_gpu_reset(
        latin_font(),
        &[("A", "B"), ("AMWXYZ", "B"), ("A", "BC")],
        false,
    );
}

#[test]
#[ignore = "requires a headless wgpu adapter and Noto/Segoe/Apple color emoji"]
fn gpu_clear_reload_matches_fresh_emoji_with_owned_pages() {
    let font = [
        "/usr/share/fonts/noto/NotoColorEmoji.ttf",
        "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        "/usr/share/fonts/TTF/NotoColorEmoji.ttf",
        "C:/Windows/Fonts/seguiemj.ttf",
        "/System/Library/Fonts/Apple Color Emoji.ttc",
    ]
    .iter()
    .find_map(|path| read_font_file(path).ok())
    .expect("color emoji font required");
    check_gpu_reset(font, &[("😀", "😁"), ("😀😂😃", "😁")], true);
}
