//! Public-API regression scenarios. No renderer or cache algorithms are changed
//! for the benchmark. See performance.md for what is timed and the span-specific controls.
mod support;

use sanscale::{
    Align, BlockKey, Color, Draw, Motion, ParagraphKey, ParagraphSource, Rect, ShapedHandle, Style,
    TextService, Vec2,
};
use serde_json::json;
use std::{borrow::Cow, hint::black_box, time::Instant};
use support::gpu::{Gpu, HEIGHT, WIDTH};
use support::{Fonts, Options, Suite, Times, hash, measure, ns, style};

struct Doc {
    text: Vec<String>,
    keys: Vec<ParagraphKey>,
    generations: Vec<u32>,
}
impl Doc {
    fn new(text: Vec<String>) -> Self {
        let keys = (0..text.len())
            .map(|i| ParagraphKey {
                namespace: 1,
                slot: i as u32,
                generation: 0,
            })
            .collect();
        let generations = vec![0; text.len()];
        Self {
            text,
            keys,
            generations,
        }
    }
    fn one(s: impl Into<String>) -> Self {
        Self::new(vec![s.into()])
    }
    fn bump(&mut self, i: usize) {
        self.keys[i].generation += 1;
        self.generations[self.keys[i].slot as usize] = self.keys[i].generation;
    }
    fn insert(&mut self, index: usize, value: String) {
        let slot = self.text.len() as u32;
        self.text.push(value);
        self.generations.push(0);
        self.keys.insert(
            index,
            ParagraphKey {
                namespace: 1,
                slot,
                generation: 0,
            },
        );
    }
    fn remove(&mut self, index: usize) -> String {
        let key = self.keys.remove(index);
        self.generations[key.slot as usize] += 1;
        std::mem::take(&mut self.text[key.slot as usize])
    }
    fn split(&mut self, index: usize) {
        let slot = self.keys[index].slot as usize;
        let mid = self.text[slot].len() / 2; // These edit fixtures are deliberately ASCII.
        let tail = self.text[slot].split_off(mid);
        self.bump(index);
        self.insert(index + 1, tail);
    }
    fn merge(&mut self, index: usize) {
        let tail = self.remove(index + 1);
        self.text[self.keys[index].slot as usize].push_str(&tail);
        self.bump(index);
    }
    fn edit_ascii(&mut self, paragraph: usize, byte: usize) {
        let s = &mut self.text[self.keys[paragraph].slot as usize];
        let replacement = if &s[byte..byte + 1] == "x" { "y" } else { "x" };
        s.replace_range(byte..byte + 1, replacement);
        self.bump(paragraph);
    }
    fn bytes(&self) -> usize {
        self.keys
            .iter()
            .map(|k| self.text[k.slot as usize].len())
            .sum::<usize>()
            + self.keys.len().saturating_sub(1)
    }
}
impl ParagraphSource for Doc {
    fn paragraph_text(&self, _: usize, key: ParagraphKey) -> Option<Cow<'_, str>> {
        // Pool identity, not the part's position in an individual shape() call.
        let index = key.slot as usize;
        (key.namespace == 1 && self.generations.get(index) == Some(&key.generation))
            .then(|| Cow::Borrowed(self.text[index].as_str()))
    }
}
fn shape(text: &mut TextService, doc: &Doc, style: &Style, block: u64) -> ShapedHandle {
    text.shape(BlockKey(block), style, &doc.keys, doc)
        .expect("valid fixture identity")
}
fn corpus_meta(doc: &Doc) -> serde_json::Value {
    json!({"bytes":doc.bytes(),"paragraphs":doc.text.len(),"sha256":hash(doc.text.join("\n").as_bytes())})
}
fn cpu_cases(suite: &mut Suite<'_>, fonts: &Fonts, unicode: &str) {
    let sizes = suite.options.sizes();
    for (name, content, wrap) in [
        ("unbroken", "a".repeat(sizes.long), Some(80.)),
        ("unbroken_no_wrap", "a".repeat(sizes.long), None),
        ("unbroken_narrow", "a".repeat(sizes.long), Some(1.)),
        ("many_words", "a b ".repeat(sizes.long / 4), Some(80.)),
        ("whitespace", " ".repeat(sizes.long), Some(80.)),
        ("combining", "e\u{301}".repeat(sizes.long / 3), Some(80.)),
        ("alternating_faces", "A日".repeat(sizes.long / 4), Some(80.)),
        ("indic", "नमस्ते ".repeat(sizes.long / 19), Some(80.)),
        ("emoji_zwj", "👩‍💻".repeat(sizes.long / 11), Some(80.)),
    ] {
        if !suite.should_run(&format!("cpu.long.{name}.fresh_layout")) {
            continue;
        }
        let mut doc = Doc::one(content);
        let (mut text, chains) = fonts.install();
        let st = style(chains.normal, wrap);
        let h = shape(&mut text, &doc, &st, 0);
        assert_eq!(text.measure(h).len_bytes(), doc.bytes());
        let params = json!({"corpus":corpus_meta(&doc),"wrap_em":wrap,"glyph_cache":"warm","paragraph_cache":"miss"});
        suite.case(
            &format!("cpu.long.{name}.fresh_layout"),
            params,
            1,
            &[("shape_calls", 1), ("assemblies", 1)],
            |_| {
                doc.bump(0);
                measure(|| {
                    black_box(shape(&mut text, &doc, &st, 0));
                    Times::default()
                })
            },
        );
    }

    for count in [1, sizes.paragraphs / 8, sizes.paragraphs] {
        let body = "word abcd efgh ijkl mnop qrst uvwx yz. ".repeat(2);
        for action in [
            "block_hit",
            "paragraph_hits_reassemble",
            "edit_first",
            "edit_middle",
            "edit_last",
            "invalidate_all_control",
            "width_fresh",
            "width_cycle_cached",
        ] {
            if !suite.should_run(&format!("cpu.paragraphs.{count}.{action}")) {
                continue;
            }
            let mut doc = Doc::new(vec![body.clone(); count]);
            let (mut text, chains) = fonts.install();
            let mut st = style(chains.normal, Some(40.));
            shape(&mut text, &doc, &st, 0);
            if action == "width_cycle_cached" {
                // Three complete width variants fit under the production cap even
                // for the stress paragraph count. Warm every variant explicitly.
                for width in [16., 40., 80.] {
                    st.wrap_em = Some(width);
                    shape(&mut text, &doc, &st, 0);
                }
            }
            let units = if action == "block_hit" { 1000 } else { 1 };
            let max_shapes = match action {
                "block_hit" | "paragraph_hits_reassemble" | "width_cycle_cached" => 0,
                "invalidate_all_control" | "width_fresh" => count as u64,
                _ => 1,
            };
            suite.case(
                &format!("cpu.paragraphs.{count}.{action}"),
                json!({"corpus":corpus_meta(&doc),"scope":"shape API; edit/key setup excluded"}),
                units,
                &[
                    ("shape_calls", max_shapes),
                    ("source_reads", max_shapes),
                    ("flow_calls", max_shapes),
                    ("assemblies", if action == "block_hit" { 0 } else { 1 }),
                ],
                |i| {
                    let block = if action == "paragraph_hits_reassemble" {
                        i as u64 + 1
                    } else {
                        0
                    };
                    match action {
                        "width_fresh" => {
                            st.wrap_em = Some([16., 40., 80.][i % 3] + (i / 3 + 1) as f32 * 0.03125)
                        }
                        "width_cycle_cached" => st.wrap_em = Some([16., 40., 80.][i % 3]),
                        "edit_first" => doc.edit_ascii(0, 0),
                        "edit_middle" => doc.edit_ascii(count / 2, 20),
                        "edit_last" => doc.edit_ascii(count - 1, doc.text[count - 1].len() - 1),
                        "invalidate_all_control" => {
                            for k in 0..count {
                                doc.bump(k);
                            }
                        }
                        _ => {}
                    }
                    measure(|| {
                        for _ in 0..units {
                            black_box(shape(&mut text, &doc, &st, block));
                        }
                        Times::default()
                    })
                },
            );
        }
    }

    for action in [
        "insert_front",
        "delete_front",
        "split_middle",
        "merge_middle",
        "empty_paragraphs",
    ] {
        let max_shapes = match action {
            "delete_front" => 0,
            "split_middle" => 2,
            "empty_paragraphs" => sizes.paragraphs as u64,
            _ => 1,
        };
        suite.case(&format!("cpu.structure.{action}"),json!({"paragraphs":sizes.paragraphs,"body_bytes":if action=="empty_paragraphs"{0}else{80},"identities":"stable across insertion/removal"}),1,&[("shape_calls",max_shapes)], |_| {
            let body=if action=="empty_paragraphs" {String::new()} else {"abcd efgh ijkl mnop ".repeat(4)};
            let mut doc=Doc::new(vec![body;sizes.paragraphs]);
            let (mut text,c)=fonts.install();let st=style(c.normal,Some(40.));
            if action!="empty_paragraphs" {shape(&mut text,&doc,&st,0);}
            match action {
                "insert_front"=>doc.insert(0,"inserted paragraph".into()),
                "delete_front"=>{doc.remove(0);},
                "split_middle"=>doc.split(sizes.paragraphs/2),
                "merge_middle"=>doc.merge(sizes.paragraphs/2),
                _=>{},
            }
            measure(|| {black_box(shape(&mut text,&doc,&st,0));Times::default()})
        });
    }

    // The same long paragraph with a one-byte edit at either end or in the middle.
    for position in [0, sizes.long / 2, sizes.long - 1] {
        if !suite.should_run(&format!("cpu.long.edit_byte_{position}")) {
            continue;
        }
        let mut doc = Doc::one("a".repeat(sizes.long));
        let (mut text, chains) = fonts.install();
        let st = style(chains.normal, Some(80.));
        shape(&mut text, &doc, &st, 0);
        suite.case(
            &format!("cpu.long.edit_byte_{position}"),
            json!({"bytes":sizes.long,"edit_bytes":1}),
            1,
            &[("shape_calls", 1)],
            |_| {
                doc.edit_ascii(0, position);
                measure(|| {
                    black_box(shape(&mut text, &doc, &st, 0));
                    Times::default()
                })
            },
        );
    }
    for action in [
        "width_fresh",
        "width_cycle_cached",
        "line_spacing_fresh",
        "chain_cached_toggle",
        "chain_fresh_layout",
        "transient_hit",
    ] {
        if !suite.should_run(&format!("cpu.cache.{action}")) {
            continue;
        }
        let mut doc = Doc::one("hello variable names and numbers 0123456789 ".repeat(64));
        let (mut text, chains) = fonts.install();
        let mut st = style(chains.normal, Some(80.));
        shape(&mut text, &doc, &st, 0);
        if action == "width_cycle_cached" {
            for width in [40., 80., 120.] {
                st.wrap_em = Some(width);
                shape(&mut text, &doc, &st, 0);
            }
        }
        if action.starts_with("chain_") {
            st.chain = chains.italic;
            shape(&mut text, &doc, &st, 0);
        }
        if action == "transient_hit" {
            text.shape_transient(&doc.text[0], &st).unwrap();
        }
        let cached = matches!(
            action,
            "width_cycle_cached" | "chain_cached_toggle" | "transient_hit"
        );
        let units = if action == "transient_hit" { 100 } else { 1 };
        suite.case(&format!("cpu.cache.{action}"),json!({"corpus":corpus_meta(&doc),"note":"whole-chain toggle, NOT inline-span coverage"}),units,&[("shape_calls",if cached {0}else{1})], |i| {
            match action {
                "width_fresh"=>st.wrap_em=Some(80.+(i+1) as f32*0.125),
                "width_cycle_cached"=>st.wrap_em=Some([40.,80.,120.][i%3]),
                "line_spacing_fresh"=>st.line_spacing=1.3+(i+1) as f32*0.03125,
                "chain_cached_toggle"=>st.chain=if i%2==0 {chains.normal}else{chains.italic},
                "chain_fresh_layout"=> {st.chain=if i%2==0 {chains.normal}else{chains.italic}; doc.bump(0);},
                _=>{},
            }
            measure(|| {
                for _ in 0..units {
                    if action=="transient_hit" { black_box(text.shape_transient(&doc.text[0],&st).unwrap()); }
                    else { black_box(shape(&mut text,&doc,&st,0)); }
                }
                Times::default()
            })
        });
    }
    for action in [
        "alignment_only_miss",
        "same_faces_new_chain",
        "long_fallback_chain",
    ] {
        suite.case(
            &format!("cpu.cache.{action}"),
            json!({"text":"A日 repeated 256","scope":"warm glyphs, new style"}),
            1,
            &[],
            |_| {
                let doc = Doc::one("A日".repeat(256));
                let (mut text, chains) = fonts.install();
                let mut st = style(chains.normal, Some(80.));
                shape(&mut text, &doc, &st, 0);
                match action {
                    "alignment_only_miss" => st.align = Align::Center,
                    "same_faces_new_chain" => st.chain = chains.duplicate,
                    _ => st.chain = chains.long_fallback,
                }
                measure(|| {
                    black_box(shape(&mut text, &doc, &st, 0));
                    Times::default()
                })
            },
        );
    }

    for shared in [false, true] {
        suite.case(
            &format!(
                "cpu.labels.{}",
                if shared {
                    "shared_block_hit"
                } else {
                    "distinct_blocks"
                }
            ),
            json!({"items":sizes.blocks,"text":"repeated label"}),
            1,
            &[],
            |_| {
                let (mut text, c) = fonts.install();
                let doc = Doc::one("repeated label");
                let st = style(c.normal, None);
                shape(&mut text, &doc, &st, 0);
                measure(|| {
                    for i in 0..sizes.blocks {
                        black_box(shape(
                            &mut text,
                            &doc,
                            &st,
                            if shared { 0 } else { i as u64 + 1 },
                        ));
                    }
                    Times::default()
                })
            },
        );
    }

    let scalars: Vec<char> = unicode.chars().collect();
    for count in [128, sizes.glyphs / 4, sizes.glyphs] {
        for group in [1, 256] {
            let doc = Doc::new(
                scalars[..count]
                    .chunks(group)
                    .map(|s| s.iter().collect())
                    .collect(),
            );
            for cold in [true, false] {
                suite.case(&format!("cpu.unicode.{count}.group_{group}.{}",if cold{"cold_glyphs"}else{"warm_glyphs"}),
                    json!({"scalars":count,"blocks":doc.text.len(),"group":group,"corpus":corpus_meta(&doc)}),1,&[], |_| {
                    let (mut text,chains)=fonts.install();
                    let st=style(chains.cjk,None);
                    if !cold { text.shape_transient(&scalars[..count].iter().collect::<String>(),&st).unwrap(); }
                    measure(|| {
                        for i in 0..doc.keys.len() {
                            black_box(text.shape(BlockKey(i as u64),&st,&doc.keys[i..i+1],&doc).unwrap());
                        }
                        Times::default()
                    })
                });
            }
        }
    }

    for query in [
        "hit_test_bottom",
        "caret_near_end",
        "selection_all",
        "vertical_motion_end",
    ] {
        if !suite.should_run(&format!("cpu.queries.{query}")) {
            continue;
        }
        let doc = Doc::new(vec![
            "words and more words on a wrapped editor line "
                .repeat(4);
            sizes.paragraphs
        ]);
        let doc_bytes = doc.bytes();
        let (mut text, chains) = fonts.install();
        let st = style(chains.normal, Some(40.));
        let h = shape(&mut text, &doc, &st, 0);
        let layout = text.measure(h);
        suite.case(
            &format!("cpu.queries.{query}"),
            json!({"bytes":doc_bytes,"lines":layout.line_count()}),
            100,
            &[("shape_calls", 0), ("flow_calls", 0)],
            |_| {
                measure(|| {
                    for _ in 0..100 {
                        match query {
                            "hit_test_bottom" => {
                                black_box(
                                    layout.hit_test(Vec2::new(10., layout.height_em() - 0.5)),
                                );
                            }
                            "caret_near_end" => {
                                black_box(layout.caret_at(doc_bytes - 1));
                            }
                            "selection_all" => {
                                black_box(layout.selection(0..doc_bytes));
                            }
                            _ => {
                                let caret = layout.caret_at(doc_bytes - 1);
                                black_box(layout.caret_move(caret, Motion::Up, &mut None, &()));
                            }
                        }
                    }
                    Times::default()
                })
            },
        );
    }
    if suite.options.tier == "stress" {
        // Actual production limits, not a special tiny test-only cache.
        for kind in ["blocks", "paragraphs"] {
            suite.case(
                &format!("cpu.eviction.{kind}"),
                json!({"entries":(1<<17)+1024,"production_limit":1<<17}),
                1,
                &[],
                |_| {
                    let (mut text, chains) = fonts.install();
                    let mut doc = Doc::one("x");
                    let st = style(chains.normal, None);
                    measure(|| {
                        for i in 0..(1 << 17) + 1024 {
                            if kind == "paragraphs" {
                                doc.bump(0);
                            }
                            black_box(shape(
                                &mut text,
                                &doc,
                                &st,
                                if kind == "blocks" { i as u64 } else { 0 },
                            ));
                        }
                        Times::default()
                    })
                },
            );
        }
    }
}

fn grid(fonts: &Fonts, unicode: &str, group: usize) -> (TextService, Vec<Draw>) {
    let chars: Vec<char> = unicode.chars().collect();
    let doc = Doc::new(chars.chunks(group).map(|c| c.iter().collect()).collect());
    let (mut text, chains) = fonts.install();
    let st = style(chains.cjk, None);
    let draws = (0..doc.keys.len())
        .map(|i| {
            let block = text
                .shape(BlockKey(i as u64), &st, &doc.keys[i..i + 1], &doc)
                .unwrap();
            let cell = i * group;
            Draw {
                block,
                at: Vec2::new((cell % 256) as f32 * 4., (cell / 256) as f32 * 4.),
                size: 4.,
                color: Color([0.9, 0.9, 0.9, 1.]),
                clip: None,
                ..Default::default()
            }
        })
        .collect();
    (text, draws)
}
fn gpu_cases(suite: &mut Suite<'_>, fonts: &Fonts, unicode: &str, gpu: &Gpu) {
    let sizes = suite.options.sizes();
    for group in [1, 256] {
        for action in [
            "prepare_warm",
            "draw_retained",
            "camera_transform",
            "draw_transient_batch",
            "draw_individual",
            "recolor",
            "move",
            "scale",
            "clip_follow",
            "clip_scroll",
            "alternating_clips",
            "alternating_color_variants",
        ] {
            if !suite.should_run(&format!("gpu.unicode.group_{group}.{action}")) {
                continue;
            }
            let (mut text, base) = grid(fonts, unicode, group);
            gpu.attach(&mut text);
            let mut draws = base.clone();
            if action == "clip_follow" {
                for d in &mut draws {
                    d.clip = Some(Rect::new(
                        d.at.x,
                        d.at.y,
                        if group == 1 { 4. } else { 1024. },
                        8.,
                    ));
                }
            }
            let retained = text.prepare(&gpu.device, &gpu.queue, &draws);
            gpu.draw_batch(&text, &retained);
            gpu.drain();
            let steady = matches!(
                action,
                "prepare_warm"
                    | "draw_retained"
                    | "camera_transform"
                    | "draw_transient_batch"
                    | "draw_individual"
                    | "move"
                    | "scale"
                    | "clip_follow"
            );
            let mut budgets = vec![("shape_calls", 0), ("flow_calls", 0)];
            if steady {
                budgets.push(("geometry_builds", 0));
            }
            if matches!(action, "draw_retained" | "camera_transform") {
                budgets.extend([
                    ("prepares", 0),
                    ("vertex_upload_bytes", 0),
                    ("text_atlas_upload_bytes", 0),
                ]);
            }
            suite.case(&format!("gpu.unicode.group_{group}.{action}"),json!({"scalars":sizes.glyphs,"items":draws.len(),"cell_px":4,"gpu":"completed offscreen frame; prepare_warm is prepare+drain only"}),1,&budgets, |i| {
                let step=(i+1) as f32;
                for (index,d) in draws.iter_mut().enumerate() {
                    let original=base[index];
                    match action {
                        "recolor"=>d.color=Color([if i%2==0{0.4}else{0.9},0.8,0.7,1.]),
                        "move"=>d.at=Vec2::new(original.at.x+step,original.at.y),
                        "scale"=>{let factor=if i%2==0 {1.}else{2.}; d.size=4.*factor; d.at=Vec2::new(original.at.x*factor,original.at.y*factor);},
                        "clip_follow"=>{d.at.x=original.at.x+step*4.;d.clip=Some(Rect::new(d.at.x,d.at.y,if group==1{4.}else{1024.},8.));},
                        "clip_scroll"=>d.clip=Some(Rect::new(step,0.,1000.,HEIGHT as f32)),
                        "alternating_clips"=>d.clip=Some(Rect::new((index%2) as f32,0.,1100.,HEIGHT as f32)),
                        _=>{},
                    }
                }
                measure(|| {
                    match action {
                        "draw_retained"=>{assert!(text.batch_live(&retained));gpu.draw_batch(&text,&retained)},
                        "camera_transform" => {
                            let matrix=glam::Mat4::from_cols_array(&TextService::pixel_ortho(WIDTH,HEIGHT))
                                * glam::Mat4::from_translation(glam::Vec3::new(step,0.,0.));
                            text.set_transform(&gpu.queue,matrix.to_cols_array());
                            assert!(text.batch_live(&retained));
                            gpu.draw_batch(&text,&retained)
                        },
                        "draw_transient_batch"=>gpu.render(|pass|text.draw_batch(&gpu.device,&gpu.queue,pass,&draws)),
                        "draw_individual"=>gpu.render(|pass| { for d in &draws {text.draw(&gpu.device,&gpu.queue,pass,d.block,d.at,d.size,d.color,d.clip);} }),
                        "prepare_warm"=>{
                            let start=Instant::now();
                            let batch=text.prepare(&gpu.device,&gpu.queue,&draws);
                            let elapsed=ns(start);
                            let wait=Instant::now();gpu.drain();
                            black_box(batch);
                            Times{prepare_ns:Some(elapsed),wait_ns:Some(ns(wait)),..Default::default()}
                        },
                        "alternating_color_variants"=>{
                            let start=Instant::now();
                            let a=text.prepare(&gpu.device,&gpu.queue,&draws);
                            let mut other=draws.clone();
                            for d in &mut other {d.color=Color([0.3,0.8,0.4,1.]);}
                            let b=text.prepare(&gpu.device,&gpu.queue,&other);
                            let prepare_ns=ns(start);
                            let mut t=gpu.render(|pass|{text.draw_prepared(pass,&a);text.draw_prepared(pass,&b);});
                            t.prepare_ns=Some(prepare_ns);t
                        },
                        _=>{
                            let start=Instant::now();
                            let batch=text.prepare(&gpu.device,&gpu.queue,&draws);
                            let prepare_ns=ns(start);
                            let mut t=gpu.draw_batch(&text,&batch);t.prepare_ns=Some(prepare_ns);t
                        },
                    }
                })
            });
        }
    }
    // Initial atlas upload is separated from font I/O, shaping, and pipeline creation.
    suite.case(
        "gpu.unicode.cold_geometry_and_upload",
        json!({"scalars":sizes.glyphs,"group":256}),
        1,
        &[("shape_calls", 0)],
        |_| {
            let (mut text, draws) = grid(fonts, unicode, 256);
            gpu.attach(&mut text);
            measure(|| {
                let start = Instant::now();
                let batch = text.prepare(&gpu.device, &gpu.queue, &draws);
                let p = ns(start);
                let mut t = gpu.draw_batch(&text, &batch);
                t.prepare_ns = Some(p);
                t
            })
        },
    );
    // Reveal traversal/assembly work hidden by a tiny visible viewport.
    for position in ["top", "middle", "bottom"] {
        if !suite.should_run(&format!("gpu.document.{position}.one_paragraph_edit")) {
            continue;
        }
        let mut doc = Doc::new(vec![
            "int value = 123; /* a short C source line */".into();
            sizes.paragraphs
        ]);
        let (mut text, chains) = fonts.install();
        let st = style(chains.normal, Some(80.));
        let h = shape(&mut text, &doc, &st, 0);
        let height = text.measure(h).height_em() * 14.;
        let offset = match position {
            "middle" => height * 0.5,
            "bottom" => (height - HEIGHT as f32).max(0.),
            _ => 0.,
        };
        let mut d = Draw {
            block: h,
            at: Vec2::new(0., -offset),
            size: 14.,
            color: Color([0.9, 0.9, 0.9, 1.]),
            clip: Some(Rect::new(0., 0., WIDTH as f32, HEIGHT as f32)),
            ..Default::default()
        };
        gpu.attach(&mut text);
        let initial = text.prepare(&gpu.device, &gpu.queue, &[d]);
        gpu.draw_batch(&text, &initial);
        suite.case(&format!("gpu.document.{position}.one_paragraph_edit"),json!({"paragraphs":sizes.paragraphs,"edit_paragraph":sizes.paragraphs/2,"viewport_height":HEIGHT}),1,&[("shape_calls",1)], |_| {
            doc.edit_ascii(sizes.paragraphs/2,4);
            measure(|| {
                let start=Instant::now();d.block=shape(&mut text,&doc,&st,0);
                let batch=text.prepare(&gpu.device,&gpu.queue,&[d]);let p=ns(start);
                let mut t=gpu.draw_batch(&text,&batch);t.prepare_ns=Some(p);t
            })
        });
    }
    for action in ["scroll", "retained"] {
        let name = format!("gpu.long_line.{action}");
        if !suite.should_run(&name) {
            continue;
        }
        let doc = Doc::one("a".repeat(sizes.long));
        let (mut text, chains) = fonts.install();
        let h = shape(&mut text, &doc, &style(chains.normal, None), 0);
        let mut draw = Draw {
            block: h,
            at: Vec2::new(0., 0.),
            size: 14.,
            color: Color([0.9, 0.9, 0.9, 1.]),
            clip: Some(Rect::new(0., 0., WIDTH as f32, HEIGHT as f32)),
            ..Default::default()
        };
        gpu.attach(&mut text);
        let retained = text.prepare(&gpu.device, &gpu.queue, &[draw]);
        gpu.draw_batch(&text, &retained);
        suite.case(
            &name,
            json!({"bytes":sizes.long,"wrap":false,"clip":"small visible fraction"}),
            1,
            &[("shape_calls", 0), ("flow_calls", 0)],
            |i| {
                draw.at.x = -((i + 1) as f32 * 16.);
                measure(|| {
                    if action == "retained" {
                        return gpu.draw_batch(&text, &retained);
                    }
                    let start = Instant::now();
                    let batch = text.prepare(&gpu.device, &gpu.queue, &[draw]);
                    let p = ns(start);
                    let mut t = gpu.draw_batch(&text, &batch);
                    t.prepare_ns = Some(p);
                    t
                })
            },
        );
    }
    for action in ["cold_raster", "same_bucket", "cross_bucket"] {
        if !suite.should_run(&format!("gpu.emoji.{action}")) {
            continue;
        }
        let doc = Doc::one("😀👩‍💻🇬🇧".repeat(32));
        let (mut text, chains) = fonts.install();
        let st = style(chains.normal, None);
        let h = shape(&mut text, &doc, &st, 0);
        let base = Draw {
            block: h,
            at: Vec2::new(0., 0.),
            size: 17.,
            color: Color([1.; 4]),
            clip: None,
            ..Default::default()
        };
        gpu.attach(&mut text);
        let batch = text.prepare(&gpu.device, &gpu.queue, &[base]);
        gpu.draw_batch(&text, &batch);
        suite.case(
            &format!("gpu.emoji.{action}"),
            json!({"sequences":96,"unique_sequences":3}),
            1,
            &[("shape_calls", 0)],
            |i| {
                // A fresh service is only used for the explicitly cold-raster case.
                let mut fresh;
                let (service, mut draw) = if action == "cold_raster" {
                    let (mut t, c) = fonts.install();
                    let block = shape(&mut t, &doc, &style(c.normal, None), 0);
                    gpu.attach(&mut t);
                    fresh = t;
                    (&mut fresh, Draw { block, ..base })
                } else {
                    (&mut text, base)
                };
                if action == "same_bucket" {
                    draw.size = if i % 2 == 0 { 18. } else { 19. };
                }
                if action == "cross_bucket" {
                    draw.size = if i % 2 == 0 { 17. } else { 65. };
                }
                measure(|| {
                    let start = Instant::now();
                    let batch = service.prepare(&gpu.device, &gpu.queue, &[draw]);
                    let p = ns(start);
                    let mut t = gpu.draw_batch(service, &batch);
                    t.prepare_ns = Some(p);
                    t
                })
            },
        );
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let options = Options::parse();
    if cfg!(debug_assertions) {
        eprintln!("WARNING: debug build; do not use for timing regressions");
    }
    let fonts = Fonts::load(&options);
    let unicode = fonts.unicode(options.sizes().glyphs);
    let mut suite = Suite::new(&options);
    cpu_cases(&mut suite, &fonts, &unicode);
    support::styled::cpu(&mut suite, &fonts);
    let gpu = options.gpu.then(Gpu::new);
    if let Some(gpu) = &gpu {
        gpu_cases(&mut suite, &fonts, &unicode, gpu);
        support::styled::gpu(&mut suite, &fonts, gpu);
    }
    suite.finish(&fonts,gpu.as_ref().map(Gpu::metadata),json!({"unicode_sha256":hash(unicode.as_bytes()),"unicode_scalars":unicode.chars().count()}));
}
