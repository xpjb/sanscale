pub mod gpu;
mod memory;
pub mod styled;

use sanscale::{Align, FontChainHandle, FontData, Style, TextService};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap, hint::black_box, path::PathBuf, process::Command, sync::Arc,
    time::Instant,
};

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub struct Options {
    pub tier: String,
    pub samples: usize,
    pub warmup: usize,
    pub only: String,
    pub gpu: bool,
    pub list: bool,
    pub out: PathBuf,
    pub fonts: BTreeMap<String, PathBuf>,
}
impl Options {
    pub fn parse() -> Self {
        let mut out = Self {
            tier: "quick".into(),
            samples: 7,
            warmup: 2,
            only: String::new(),
            gpu: false,
            list: false,
            out: "perf-results/latest.json".into(),
            fonts: BTreeMap::new(),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--tier" => out.tier = args.next().expect("--tier VALUE"),
                "--samples" => {
                    out.samples = args.next().unwrap().parse().expect("positive sample count")
                }
                "--warmup" => out.warmup = args.next().unwrap().parse().expect("warmup count"),
                "--only" => out.only = args.next().expect("--only SUBSTRING"),
                "--out" => out.out = args.next().expect("--out FILE").into(),
                "--gpu" => out.gpu = true,
                "--list" => out.list = true,
                "--bench" => {} // accepted if supplied by Cargo
                "--help" | "-h" => {
                    println!(
                        "pathological [--tier quick|standard|stress] [--samples N] [--warmup N]\n  [--only SUBSTRING] [--gpu] [--list] [--out FILE]\n  [--latin-font FILE] [--italic-font FILE] [--cjk-font FILE]\n  [--emoji-font FILE] [--indic-font FILE]\n\nFonts use face index 0. Stress deliberately includes very expensive cases.\nUse --only to isolate them. No window is opened. See performance.md."
                    );
                    std::process::exit(0);
                }
                name if name.ends_with("-font") => {
                    out.fonts.insert(
                        name.trim_start_matches("--")
                            .trim_end_matches("-font")
                            .into(),
                        args.next().expect("font path").into(),
                    );
                }
                _ => panic!("unknown argument {arg}; use --help"),
            }
        }
        assert!(
            matches!(out.tier.as_str(), "quick" | "standard" | "stress"),
            "unknown tier"
        );
        assert!(out.samples > 0, "need at least one sample");
        out
    }
    pub fn sizes(&self) -> Sizes {
        match self.tier.as_str() {
            "quick" => Sizes {
                glyphs: 2048,
                long: 8192,
                paragraphs: 256,
                blocks: 1024,
            },
            "standard" => Sizes {
                glyphs: 16384,
                long: 65536,
                paragraphs: 4096,
                blocks: 8192,
            },
            _ => Sizes {
                glyphs: 41472,
                long: 1_000_000,
                paragraphs: 32768,
                blocks: 41472,
            },
        }
    }
}
#[derive(Clone, Copy)]
pub struct Sizes {
    pub glyphs: usize,
    pub long: usize,
    pub paragraphs: usize,
    pub blocks: usize,
}

struct Fixture {
    name: &'static str,
    path: PathBuf,
    bytes: FontData,
    sha256: String,
}
pub struct Fonts {
    fixtures: Vec<Fixture>,
}
#[derive(Clone, Copy)]
pub struct Chains {
    pub normal: FontChainHandle,
    pub italic: FontChainHandle,
    pub cjk: FontChainHandle,
    pub duplicate: FontChainHandle,
    pub long_fallback: FontChainHandle,
}
impl Fonts {
    pub fn load(options: &Options) -> Self {
        let choices: &[(&str, &[&str])] = &[
            (
                "latin",
                &[
                    "/usr/share/fonts/TTF/DejaVuSans.ttf",
                    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                ],
            ),
            (
                "italic",
                &[
                    "/usr/share/fonts/TTF/DejaVuSans-Oblique.ttf",
                    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Oblique.ttf",
                ],
            ),
            (
                "cjk",
                &[
                    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
                    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                ],
            ),
            (
                "emoji",
                &[
                    "/usr/share/fonts/noto/NotoColorEmoji.ttf",
                    "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
                ],
            ),
            (
                "indic",
                &[
                    "/usr/share/fonts/noto/NotoSansDevanagari-Regular.ttf",
                    "/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf",
                ],
            ),
        ];
        let fixtures = choices.iter().map(|&(name, paths)| {
            let path = options.fonts.get(name).cloned().or_else(|| paths.iter().map(PathBuf::from).find(|p| p.exists()))
                .unwrap_or_else(|| panic!("missing {name} fixture; pass --{name}-font FILE (no silent font substitution)"));
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let sha256 = hash(&bytes);
            ttf_parser::Face::parse(&bytes, 0).expect("font fixture face 0");
            Fixture { name, path, bytes: Arc::new(bytes), sha256 }
        }).collect();
        Self { fixtures }
    }
    pub fn install(&self) -> (TextService, Chains) {
        let mut text = TextService::new();
        let fonts: Vec<_> = self
            .fixtures
            .iter()
            .map(|f| text.map_font(f.bytes.clone(), 0).unwrap())
            .collect();
        let normal = text.register_chain(&[fonts[0], fonts[2], fonts[4], fonts[3]]);
        let italic = text.register_chain(&[fonts[1], fonts[2], fonts[4], fonts[3]]);
        let duplicate = text.register_chain(&[fonts[0], fonts[2], fonts[4], fonts[3]]);
        let cjk = text.register_chain(&[fonts[2]]);
        let mut long = vec![fonts[0]; 16];
        long.extend([fonts[2], fonts[4], fonts[3]]);
        let long_fallback = text.register_chain(&long);
        (
            text,
            Chains {
                normal,
                italic,
                cjk,
                duplicate,
                long_fallback,
            },
        )
    }
    pub fn unicode(&self, count: usize) -> String {
        let f = &self.fixtures[2];
        let face = ttf_parser::Face::parse((*f.bytes).as_ref(), 0).unwrap();
        let chars: String = (0x3400..=0x9fff)
            .chain(0x20000..=0x3134f)
            // Large grids also use precomposed Hangul and spacing ideographs/
            // radicals. Do not pad with combining marks: grouped and separate
            // layouts would then have different shaping semantics.
            .chain(0xac00..=0xd7a3)
            .chain(0xf900..=0xfaff)
            .chain(0x2e80..=0x2fff)
            .filter_map(char::from_u32)
            .filter(|&c| face.glyph_index(c).is_some_and(|g| g.0 != 0))
            .take(count)
            .collect();
        assert_eq!(
            chars.chars().count(),
            count,
            "CJK fixture lacks enough distinct covered scalars for this tier"
        );
        chars
    }
    pub fn metadata(&self) -> Value {
        json!(
            self.fixtures
                .iter()
                .map(
                    |f| json!({"role": f.name, "path": f.path, "face_index": 0, "sha256": f.sha256})
                )
                .collect::<Vec<_>>()
        )
    }
}
pub fn style(chain: FontChainHandle, wrap_em: Option<f32>) -> Style {
    Style {
        chain,
        wrap_em,
        align: Align::Left,
        line_spacing: 1.2,
    }
}

#[derive(Default)]
pub struct Times {
    pub prepare_ns: Option<u64>,
    pub encode_ns: Option<u64>,
    pub submit_ns: Option<u64>,
    pub wait_ns: Option<u64>,
    pub gpu_pass_ns: Option<u64>,
}
pub fn ns(t: Instant) -> u64 {
    t.elapsed().as_nanos().try_into().unwrap()
}

pub struct Sample {
    pub total_ns: u64,
    pub times: Times,
    pub work: BTreeMap<String, u64>,
    pub memory: Value,
}
/// Setup, corpus generation, font I/O, and result serialization are outside this scope.
pub fn measure(f: impl FnOnce() -> Times) -> Sample {
    #[cfg(feature = "perf-counters")]
    {
        // Setup is untimed, not exempt from corpus validity checks. In GPU
        // scenarios most initial shaping happens before this measurement.
        let setup = sanscale::profiling::work_counters();
        assert_eq!(
            setup.missing_glyphs, 0,
            "missing glyphs during fixture setup"
        );
        assert_eq!(setup.emoji_drops, 0, "dropped emoji during fixture setup");
        sanscale::profiling::reset_work_counters();
    }
    memory::start();
    let start = Instant::now();
    let times = black_box(f());
    let total_ns = ns(start);
    let mem = memory::stop();
    let work = work_snapshot();
    Sample {
        total_ns,
        times,
        work,
        memory: mem,
    }
}
fn work_snapshot() -> BTreeMap<String, u64> {
    #[cfg(feature = "perf-counters")]
    {
        sanscale::profiling::work_counters()
            .values()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }
    #[cfg(not(feature = "perf-counters"))]
    {
        BTreeMap::new()
    }
}

pub struct Suite<'a> {
    pub options: &'a Options,
    pub results: Vec<Value>,
}
impl<'a> Suite<'a> {
    pub fn new(options: &'a Options) -> Self {
        Self {
            options,
            results: Vec::new(),
        }
    }
    pub fn should_run(&self, name: &str) -> bool {
        if !name.contains(&self.options.only) {
            return false;
        }
        if self.options.list {
            println!("{name}");
            return false;
        }
        true
    }
    /// The callback performs untimed setup and returns an explicitly measured operation.
    /// Budgets are upper bounds on work, not wall-clock thresholds or demands to remain slow.
    pub fn case(
        &mut self,
        name: &str,
        params: Value,
        units: usize,
        budgets: &[(&str, u64)],
        mut run: impl FnMut(usize) -> Sample,
    ) {
        if !self.should_run(name) {
            return;
        }
        eprintln!("{name}");
        let mut samples = Vec::new();
        for i in 0..self.options.warmup + self.options.samples {
            let s = run(i);
            if cfg!(feature = "perf-counters") {
                for &(key, max) in budgets {
                    assert!(
                        s.work[key] <= max,
                        "{name}: {key}={} exceeds work budget {max}",
                        s.work[key]
                    );
                }
                assert_eq!(
                    s.work["missing_glyphs"], 0,
                    "{name}: missing glyphs invalidate this corpus result"
                );
                assert_eq!(
                    s.work["emoji_drops"], 0,
                    "{name}: silently dropped emoji invalidate this result"
                );
                if name == "cpu.eviction.blocks" {
                    assert!(
                        s.work["block_evictions"] > 0,
                        "scenario did not reach the production block cap; update its workload if the cap changed"
                    );
                }
                if name == "cpu.eviction.paragraphs" {
                    assert!(
                        s.work["paragraph_evictions"] > 0,
                        "scenario did not reach the production paragraph cap; update its workload if the cap changed"
                    );
                }
                if name.starts_with("gpu.") && !name.ends_with("prepare_warm") {
                    assert!(
                        s.work["text_draw_calls"] + s.work["emoji_draw_calls"] > 0,
                        "{name}: no draw commands; do not benchmark an accidentally empty frame"
                    );
                }
            }
            if i >= self.options.warmup {
                samples.push(json!({"total_ns":s.total_ns,
                    "prepare_ns":s.times.prepare_ns, "encode_ns":s.times.encode_ns,
                    "submit_ns":s.times.submit_ns, "wait_ns":s.times.wait_ns,
                    "gpu_pass_ns":s.times.gpu_pass_ns, "work":s.work, "memory":s.memory}));
            }
        }
        let mut totals: Vec<_> = samples
            .iter()
            .map(|s| s["total_ns"].as_u64().unwrap())
            .collect();
        totals.sort_unstable();
        let median = totals[totals.len() / 2] as f64 / units as f64 / 1000.;
        eprintln!("  median {median:.3} us/op ({units} operation(s)/sample)");
        self.results
            .push(json!({"name":name,"params":params,"units":units,"samples":samples}));
    }
    pub fn finish(self, fonts: &Fonts, gpu: Option<Value>, corpora: Value) {
        if self.options.list {
            return;
        }
        assert!(!self.results.is_empty(), "--only matched no cases");
        let cmd = |args: &[&str]| -> String {
            Command::new(args[0])
                .args(&args[1..])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .unwrap_or_default()
        };
        let cpu = std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .map(str::to_owned)
        });
        let fingerprint = |root: &str| {
            fn collect(path: &std::path::Path, files: &mut Vec<PathBuf>) {
                if path.is_dir() {
                    for entry in std::fs::read_dir(path).unwrap() {
                        collect(&entry.unwrap().path(), files);
                    }
                } else {
                    // Include WGSL and any other source assets, not just Rust.
                    files.push(path.to_owned());
                }
            }
            let mut files = Vec::new();
            collect(std::path::Path::new(root), &mut files);
            files.sort();
            let mut bytes = Vec::new();
            for path in files {
                bytes.extend_from_slice(path.to_string_lossy().as_bytes());
                bytes.push(0);
                let contents = std::fs::read(path).unwrap();
                bytes.extend_from_slice(&(contents.len() as u64).to_le_bytes());
                bytes.extend(contents);
            }
            hash(&bytes)
        };
        let result = json!({"schema":1,"suite_version":1,
            "mode": if cfg!(feature="perf-counters") {"work"} else {"timing"},
            "tier":self.options.tier,"warmup":self.options.warmup,
            "metadata": {"git_commit":cmd(&["git","rev-parse","HEAD"]),
                "git_status":cmd(&["git","status","--short"]),
                "source_sha256":fingerprint("src"),"shader_sha256":fingerprint("src/shaders"),"benchmark_sha256":fingerprint("benches"),
                "fingerprint_scope":"all recursive files, including shaders; runtime source snapshot",
                "executable_sha256":std::env::current_exe().ok().and_then(|p|std::fs::read(p).ok()).map(|b|hash(&b)),
                "rustc":cmd(&["rustc","-Vv"]),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,
                "cpu":cpu,"kernel":cmd(&["uname","-sr"]),
                "debug_assertions":cfg!(debug_assertions),"rustflags":std::env::var("RUSTFLAGS").unwrap_or_default(),
                "lock_sha256":std::fs::read("Cargo.lock").ok().map(|b|hash(&b)),
                "manifest_sha256":std::fs::read("Cargo.toml").ok().map(|b|hash(&b)),
                "type_sizes":{"Draw":std::mem::size_of::<sanscale::Draw>(),"Style":std::mem::size_of::<sanscale::Style>(),
                    "ParagraphKey":std::mem::size_of::<sanscale::ParagraphKey>(),"ShapedHandle":std::mem::size_of::<sanscale::ShapedHandle>(),
                    "TextService":std::mem::size_of::<sanscale::TextService>()},
                "fonts":fonts.metadata(),"gpu":gpu},
            "corpora":corpora,"results":self.results,
            "pending_features":["retained Markdown table dependency invalidation", "context-safe reuse within a font-restyled paragraph (not implemented)", "additional combined span/capacity stress scenarios"]});
        if let Some(parent) = self
            .options
            .out
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            &self.options.out,
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
        eprintln!("wrote {}", self.options.out.display());
    }
}
