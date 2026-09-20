//! Small feature-local CPU probe, not the library's before/after benchmark suite.
//! Setup/font discovery are untimed; rendering is checked separately by --dump.
use super::{
    fonts,
    markdown::Document,
    preview::{Preview, Theme},
};
use sanscale::TextService;
use std::time::Instant;

pub fn run(font: Option<&str>) {
    let mut text = TextService::new();
    let faces = fonts::load(&mut text, font);
    let mut source = "| Step | State | Detail |\n| --- | :---: | --- |\n".to_owned();
    for i in 0..2000 {
        source.push_str(&format!("| step {i} | **done** | `stable cells` |\n"));
    }
    let mut doc = Document::new(&source);
    let mut view = Preview::new(60000);
    view.sync(&doc, &mut text, faces, Theme::default(), 720., 17.);
    let mut results = Vec::new();
    for action in [
        "append_row",
        "edit_middle_cell",
        "palette_only",
        "uncached_width",
        "full_parse_control",
    ] {
        let mut samples = Vec::new();
        for i in 0..14 {
            let start = Instant::now();
            let mut theme = Theme::default();
            let mut width = 720.;
            let mut parse_ns = 0;
            let mut parser_work = super::markdown::Work::default();
            match action {
                "append_row" => {
                    doc.append(&format!(
                        "| step {} | **done** | `stable cells` |\n",
                        2000 + i
                    ))
                    .unwrap();
                    parse_ns = start.elapsed().as_nanos() as u64;
                    parser_work = doc.last_change().work;
                }
                "edit_middle_cell" => {
                    let current = doc.source().to_string();
                    let at = current.find("step 1000").unwrap();
                    let end = at + current[at..].find('\n').unwrap();
                    let note = at + current[at..end].find("stable ").unwrap();
                    let begin = Instant::now();
                    doc.edit(
                        note..note + 12,
                        if i % 2 == 0 {
                            "stable cells"
                        } else {
                            "stable CELLS"
                        },
                    )
                    .unwrap();
                    parse_ns = begin.elapsed().as_nanos() as u64;
                    parser_work = doc.last_change().work;
                }
                "palette_only" => theme.alternate = i % 2 == 0,
                "uncached_width" => width = 721. + i as f32 * 0.73,
                "full_parse_control" => {
                    let source = doc.source().to_string();
                    let begin = Instant::now();
                    let cold = Document::new(&source);
                    parse_ns = begin.elapsed().as_nanos() as u64;
                    parser_work = cold.last_change().work;
                    std::hint::black_box(cold);
                }
                _ => unreachable!(),
            }
            let begin = Instant::now();
            let layout = if action == "full_parse_control" {
                Default::default()
            } else {
                view.sync(&doc, &mut text, faces, theme, width, 17.)
            };
            let layout_ns = if action == "full_parse_control" {
                0
            } else {
                begin.elapsed().as_nanos() as u64
            };
            if i >= 3 {
                if action == "append_row" {
                    assert_eq!(doc.last_change().work.projected_elements, 3);
                    assert_eq!(layout.measured_rows, 1);
                    assert_eq!(layout.layout_requests, 3);
                }
                if action == "edit_middle_cell" {
                    assert_eq!(doc.last_change().work.projected_elements, 1);
                    assert_eq!(layout.layout_requests, 1);
                }
                if action == "palette_only" {
                    assert_eq!(layout.layout_requests, 0);
                    assert!(layout.paint_snapshots >= 2000);
                }
                if action == "full_parse_control" {
                    assert!(parser_work.classified_lines >= 2000);
                    assert!(parser_work.projected_elements >= 6000);
                }
                samples.push(serde_json::json!({"parse_ns":parse_ns,"layout_ns":layout_ns,"classified_lines":parser_work.classified_lines,"projected_elements":parser_work.projected_elements,"layout_requests":layout.layout_requests,"paint_snapshots":layout.paint_snapshots,"measured_rows":layout.measured_rows,"indexed_rows":layout.indexed_rows}));
            }
        }
        let median = |key: &str| {
            let mut v = samples
                .iter()
                .map(|s| s[key].as_u64().unwrap())
                .collect::<Vec<_>>();
            v.sort();
            v[v.len() / 2] as f64 / 1000.
        };
        eprintln!(
            "{action}: parse {:.2} us, layout {:.2} us (median, 11 samples)",
            median("parse_ns"),
            median("layout_ns")
        );
        results.push(serde_json::json!({"case":action,"samples":samples}));
    }
    view.release(&mut text);
    println!("{}",serde_json::to_string_pretty(&serde_json::json!({"scope":"Markdown parser + CPU layout adapter; not end-to-end/GPU latency","instrumented":cfg!(feature="perf-counters"),"font_family_override":font,"initial_rows":2000,"columns":3,"warmup":3,"samples":11,"results":results})).unwrap());
}
