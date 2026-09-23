//! Window-independent sanscale adapter. Layout, projected/source coordinates,
//! table dependency tracking, and paint ownership are separate from the demo UI.
use super::markdown::{
    self as md, Content, Document, Element, Id, Origin, RawLine, TextKind,
    inline::{self, RichText},
};
use sanscale::{
    Align, BlockKey, Color, Draw, FontChainHandle, FontSpan, PaintHandle, PaintSpan, ParagraphKey,
    ParagraphSource, Rect, ShapedHandle, Style, TextService, Vec2,
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ops::Range,
    sync::Arc,
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Faces {
    pub prose: [FontChainHandle; 4],
    pub mono: [FontChainHandle; 4],
}
impl Faces {
    fn face(self, flags: u8) -> FontChainHandle {
        let index = (flags & (inline::STRONG | inline::EMPHASIS)) as usize;
        if flags & inline::CODE != 0 {
            self.mono[index]
        } else {
            self.prose[index]
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub alternate: bool,
    pub italic: bool,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            alternate: false,
            italic: true,
        }
    }
}
impl Theme {
    pub fn foreground(self) -> Color {
        Color([0.82, 0.85, 0.90, 1.])
    }
    fn role(self, flags: u8) -> Color {
        match flags {
            f if f & inline::LINK != 0 => {
                if self.alternate {
                    Color([0.96, 0.53, 0.34, 1.])
                } else {
                    Color([0.30, 0.66, 0.96, 1.])
                }
            }
            f if f & inline::CODE != 0 => {
                if self.alternate {
                    Color([0.79, 0.63, 0.96, 1.])
                } else {
                    Color([0.48, 0.83, 0.69, 1.])
                }
            }
            f if f & inline::IMAGE != 0 => Color([0.68, 0.62, 0.48, 1.]),
            _ => self.foreground(),
        }
    }
    pub fn accent(self) -> Color {
        self.role(inline::LINK)
    }
    fn panel(self) -> Color {
        Color([0.025, 0.036, 0.055, 1.])
    }
    fn grid(self) -> Color {
        Color([0.11, 0.16, 0.22, 1.])
    }
}
#[derive(Clone, Copy, Default, Debug)]
pub struct Work {
    pub paint_snapshots: usize,
    pub resolved_elements: usize,
    pub layout_requests: usize,
    pub measured_rows: usize,
    pub indexed_rows: usize,
}

/// Dynamic prefix sums: a changed cell updates one row height, not the Y
/// position of every subsequent row. Appending rows is O(log rows); inserting
/// or deleting rows in the middle rebuilds this *metadata*, not their layouts.
#[derive(Default)]
struct Heights {
    values: Vec<f32>,
    tree: Vec<f32>,
}
impl Heights {
    fn from(values: Vec<f32>) -> Self {
        let mut h = Self::default();
        for x in values {
            h.push(x);
        }
        h
    }
    fn prefix(&self, mut count: usize) -> f32 {
        let mut sum = 0.;
        while count > 0 {
            sum += self.tree[count - 1];
            count &= count - 1;
        }
        sum
    }
    fn total(&self) -> f32 {
        self.prefix(self.values.len())
    }
    fn push(&mut self, value: f32) {
        let n = self.values.len() + 1;
        let low = n & n.wrapping_neg();
        let sum = self.prefix(n - 1) - self.prefix(n - low) + value;
        self.values.push(value);
        self.tree.push(sum);
    }
    fn set(&mut self, index: usize, value: f32) {
        let delta = value - self.values[index];
        self.values[index] = value;
        let mut i = index + 1;
        while i <= self.tree.len() {
            self.tree[i - 1] += delta;
            i += i & i.wrapping_neg();
        }
    }
    fn row_at(&self, y: f32) -> usize {
        let mut index = 0;
        let mut sum = 0.;
        let mut step = self.tree.len().next_power_of_two();
        while step > 0 {
            let next = index + step;
            if next <= self.tree.len() && sum + self.tree[next - 1] <= y {
                index = next;
                sum += self.tree[next - 1];
            }
            step >>= 1;
        }
        index.min(self.values.len())
    }
    fn splice(&mut self, range: Range<usize>, count: usize) -> usize {
        if range.start == self.values.len() {
            for _ in 0..count {
                self.push(0.);
            }
            return count;
        }
        if range.len() == count {
            for i in range {
                self.set(i, 0.);
            }
            return count;
        }
        let mut values = std::mem::take(&mut self.values);
        values.splice(range, std::iter::repeat_n(0., count));
        let work = values.len();
        *self = Self::from(values);
        work
    }
}
struct Source<'a> {
    rich: &'a RichText,
    fonts: &'a [FontSpan],
    parts: Vec<Range<usize>>,
}
impl<'a> Source<'a> {
    fn new(rich: &'a RichText, fonts: &'a [FontSpan]) -> Self {
        let mut start = 0;
        let parts = rich
            .text
            .split('\n')
            .map(|s| {
                let r = start..start + s.len();
                start += s.len() + 1;
                r
            })
            .collect();
        Self { rich, fonts, parts }
    }
}
impl ParagraphSource for Source<'_> {
    fn paragraph_text(&self, i: usize, _: ParagraphKey) -> Option<Cow<'_, str>> {
        self.parts
            .get(i)
            .map(|r| Cow::Borrowed(&self.rich.text[r.clone()]))
    }
    fn paragraph_fonts(&self, i: usize, _: ParagraphKey) -> Cow<'_, [FontSpan]> {
        let r = &self.parts[i];
        Cow::Owned(
            self.fonts
                .iter()
                .filter_map(|s| {
                    let a = s.range.start.max(r.start);
                    let b = s.range.end.min(r.end);
                    (a < b).then(|| FontSpan {
                        range: a - r.start..b - r.start,
                        chain: s.chain,
                    })
                })
                .collect(),
        )
    }
}
struct TextCache {
    rich: Arc<RichText>,
    origins: Vec<RawLine>,
    fonts: Vec<FontSpan>,
    paint_spans: Vec<PaintSpan>,
    paint: Option<PaintHandle>,
    handle: ShapedHandle,
    style: Style,
    generation: u32,
    size: f32,
    height: f32,
    flags: u8,
    color: Color,
    faces: Faces,
    theme: Theme,
}
impl TextCache {
    fn shape(&mut self, text: &mut TextService, key: u64, work: &mut Work) {
        let source = Source::new(&self.rich, &self.fonts);
        let keys = (0..source.parts.len())
            .map(|i| ParagraphKey {
                namespace: key,
                slot: i as u32,
                generation: self.generation,
            })
            .collect::<Vec<_>>();
        self.handle = text
            .shape(BlockKey(key), &self.style, &keys, &source)
            .expect("Markdown adapter emits valid grapheme-aligned font spans");
        self.height = text.measure(self.handle).height_em() * self.size;
        work.layout_requests += 1;
    }
}
#[derive(Clone, Copy)]
struct Part {
    id: Id,
    x: f32,
    y: f32,
}
struct TableCache {
    align: Vec<md::Alignment>,
    width: f32,
    rows: Vec<Id>,
    heights: Heights,
}
enum LayoutContent {
    Parts {
        items: Vec<Part>,
        quote: bool,
        code: bool,
        marker: Option<String>,
    },
    Table(TableCache),
    Rule,
}
struct BlockCache {
    y: f32,
    height: f32,
    content: LayoutContent,
}
#[derive(Clone, Copy, Debug)]
pub struct Decoration {
    pub rect: Rect,
    pub color: Color,
}
#[derive(Default)]
pub struct Scene {
    pub draws: Vec<Draw>,
    pub under: Vec<Decoration>,
    pub over: Vec<Decoration>,
    placed: Vec<(Id, Vec2)>,
}

/// A view reserves BlockKey/ParagraphKey namespace `(namespace << 32) | id`.
/// Give separate documents/views distinct nonzero namespaces. Explicit `release`
/// frees its paint snapshots before discarding the view or replacing its document.
pub struct Preview {
    namespace: u32,
    document: Option<u64>,
    texts: HashMap<Id, TextCache>,
    blocks: HashMap<Id, BlockCache>,
    order: Vec<Id>,
    revision: Option<u64>,
    config: Option<(Faces, Theme, f32, f32)>,
    next_generation: u32,
    pub height: f32,
    pub width: f32,
    pub last_work: Work,
}
impl Preview {
    pub fn new(namespace: u32) -> Self {
        assert!(
            namespace > 0 && namespace < u32::MAX,
            "reserve a nonzero, non-transient namespace"
        );
        Self {
            namespace,
            document: None,
            texts: HashMap::new(),
            blocks: HashMap::new(),
            order: Vec::new(),
            revision: None,
            config: None,
            next_generation: 0,
            height: 0.,
            width: 0.,
            last_work: Work::default(),
        }
    }
    fn key(&self, id: Id) -> u64 {
        (u64::from(self.namespace) << 32) | u64::from(id)
    }
    pub fn release(&mut self, text: &mut TextService) {
        for c in self.texts.values() {
            if let Some(h) = c.paint {
                text.drop_paint(h);
            }
        }
        self.texts.clear();
        self.blocks.clear();
        self.order.clear();
        self.revision = None;
        self.config = None;
        self.document = None;
    }
    fn ensure(
        &mut self,
        e: &Element,
        text: &mut TextService,
        faces: Faces,
        theme: Theme,
        size: f32,
        width: f32,
        align: Align,
        flags: u8,
        work: &mut Work,
    ) -> f32 {
        let effective = |f| {
            if theme.italic {
                f
            } else {
                f & !inline::EMPHASIS
            }
        };
        let base = faces.face(effective(flags));
        let style = Style {
            chain: base,
            wrap_em: Some((width / size).max(0.5)),
            align,
            line_spacing: 1.25,
        };
        if let Some(c) = self.texts.get_mut(&e.id) {
            if Arc::ptr_eq(&c.rich, &e.rich)
                && c.faces == faces
                && c.theme == theme
                && c.style == style
                && c.size == size
                && c.flags == flags
                && text.measure(c.handle).line_count() > 0
            {
                c.origins = e.origins.clone();
                return c.height;
            }
        }
        work.resolved_elements += 1;
        let mut fonts: Vec<FontSpan> = Vec::new();
        let mut run = 0;
        for (at, g) in e.rich.text.grapheme_indices(true) {
            while run < e.rich.runs.len() && e.rich.runs[run].range.end <= at {
                run += 1;
            }
            let f = e.rich.runs.get(run).map_or(flags, |r| flags | r.flags);
            let face = faces.face(effective(f));
            if face != base {
                if let Some(last) = fonts
                    .last_mut()
                    .filter(|s| s.chain == face && s.range.end == at)
                {
                    last.range.end = at + g.len();
                } else {
                    fonts.push(FontSpan {
                        range: at..at + g.len(),
                        chain: face,
                    });
                }
            }
        }
        let color = theme.role(flags);
        let mut paint_spans: Vec<PaintSpan> = Vec::new();
        for r in &e.rich.runs {
            let c = theme.role(r.flags | flags);
            if c == color {
                continue;
            }
            if let Some(last) = paint_spans
                .last_mut()
                .filter(|s| s.color == c && s.range.end == r.range.start)
            {
                last.range.end = r.range.end;
            } else {
                paint_spans.push(PaintSpan {
                    range: r.range.clone(),
                    color: c,
                });
            }
        }
        let key = self.key(e.id);
        let fresh = !self.texts.contains_key(&e.id);
        let c = self.texts.entry(e.id).or_insert_with(|| TextCache {
            rich: Arc::new(RichText::default()),
            origins: Vec::new(),
            fonts: Vec::new(),
            paint_spans: Vec::new(),
            paint: None,
            handle: ShapedHandle::INVALID,
            style,
            generation: 0,
            size,
            height: 0.,
            flags,
            color,
            faces,
            theme,
        });
        let input_changed = fresh || c.rich.text != e.rich.text || c.fonts != fonts;
        let shape = input_changed || c.style != style || text.measure(c.handle).line_count() == 0;
        if input_changed {
            self.next_generation = self
                .next_generation
                .checked_add(1)
                .expect("Markdown view generation capacity");
            c.generation = self.next_generation;
        }
        if c.paint_spans != paint_spans {
            let next = if paint_spans.is_empty() {
                None
            } else {
                work.paint_snapshots += 1;
                Some(
                    text.register_paint(&paint_spans)
                        .expect("Markdown paint pool"),
                )
            };
            if let Some(old) = c.paint {
                text.drop_paint(old);
            }
            c.paint = next;
            c.paint_spans = paint_spans;
        }
        c.rich = e.rich.clone();
        c.origins = e.origins.clone();
        c.fonts = fonts;
        c.style = style;
        c.size = size;
        c.flags = flags;
        c.color = color;
        c.faces = faces;
        c.theme = theme;
        if shape {
            c.shape(text, key, work);
        } else {
            c.height = text.measure(c.handle).height_em() * size;
        }
        c.height
    }
    fn measure_row(
        &mut self,
        row: &md::Row,
        header: bool,
        table: &TableCache,
        text: &mut TextService,
        faces: Faces,
        theme: Theme,
        size: f32,
        work: &mut Work,
    ) -> f32 {
        work.measured_rows += 1;
        let width = table.width / table.align.len() as f32;
        let mut height = size * 1.25;
        for (i, e) in row.cells.iter().enumerate() {
            let align = match table.align[i] {
                md::Alignment::Left => Align::Left,
                md::Alignment::Center => Align::Center,
                md::Alignment::Right => Align::Right,
            };
            height = height.max(self.ensure(
                e,
                text,
                faces,
                theme,
                size,
                width - 20.,
                align,
                if header { inline::STRONG } else { 0 },
                work,
            ));
        }
        height + 16.
    }
    /// No parsing occurs here. Stable column widths depend on viewport/schema,
    /// never newly streamed cell contents. Width/font changes may legitimately
    /// reflow cells; a changed row height only updates prefix-sum metadata.
    pub fn sync(
        &mut self,
        doc: &Document,
        text: &mut TextService,
        faces: Faces,
        theme: Theme,
        width: f32,
        size: f32,
    ) -> Work {
        assert!(
            width.is_finite() && width > 0. && size.is_finite() && size > 0.,
            "positive finite Markdown viewport/font size"
        );
        if self.document != Some(doc.identity()) {
            self.release(text);
            self.document = Some(doc.identity());
        }
        let config = (faces, theme, width, size);
        if self.revision == Some(doc.revision()) && self.config == Some(config) {
            return Work::default();
        }
        let mut work = Work::default();
        let config_changed = self.config != Some(config);
        let changes = self
            .revision
            .and_then(|r| doc.changes_since(r))
            .map(|c| c.collect::<Vec<_>>());
        let all = config_changed || changes.is_none();
        let mut changed = HashSet::new();
        let mut reconcile = HashSet::new();
        let mut dirty_rows: HashMap<Id, HashSet<Id>> = HashMap::new();
        if let Some(changes) = &changes {
            for c in changes {
                changed.extend(c.changed_blocks.iter().copied());
                reconcile.extend(c.reconcile.iter().copied());
                for &(b, r) in &c.dirty_rows {
                    dirty_rows.entry(b).or_default().insert(r);
                }
                for id in &c.removed_elements {
                    if let Some(old) = self.texts.remove(id) {
                        if let Some(p) = old.paint {
                            text.drop_paint(p);
                        }
                    }
                }
            }
        }
        let live = doc.blocks().iter().map(|b| b.id).collect::<HashSet<_>>();
        self.blocks.retain(|id, _| live.contains(id));
        // A viewer which missed the bounded change history reconciles liveness
        // from the current document, rather than displaying lost updates.
        if changes.is_none() {
            let live = doc
                .blocks()
                .iter()
                .flat_map(|b| b.elements().map(|e| e.id))
                .collect::<HashSet<_>>();
            self.texts.retain(|id, c| {
                if live.contains(id) {
                    true
                } else {
                    if let Some(p) = c.paint {
                        text.drop_paint(p);
                    }
                    false
                }
            });
        }
        for block in doc.blocks() {
            if !all && !changed.contains(&block.id) && self.blocks.contains_key(&block.id) {
                continue;
            }
            let old = self.blocks.remove(&block.id);
            let (content, height) = match &block.content {
                Content::Table(model) => {
                    let table_width = width.max(model.align.len() as f32 * size * 6.);
                    let mut t = match old.map(|b| b.content) {
                        Some(LayoutContent::Table(t)) => t,
                        _ => TableCache {
                            align: Vec::new(),
                            width: 0.,
                            rows: Vec::new(),
                            heights: Heights::default(),
                        },
                    };
                    let full = all
                        || reconcile.contains(&block.id)
                        || t.align != model.align
                        || t.width != table_width;
                    t.align = model.align.clone();
                    t.width = table_width;
                    if full {
                        t.rows = model.rows.iter().map(|r| r.id).collect();
                        t.heights = Heights::default();
                        for (i, row) in model.rows.iter().enumerate() {
                            let h = self.measure_row(
                                row,
                                i == 0,
                                &t,
                                text,
                                faces,
                                theme,
                                size,
                                &mut work,
                            );
                            t.heights.push(h);
                        }
                        work.indexed_rows += t.rows.len();
                    } else {
                        for c in changes.as_ref().unwrap() {
                            for splice in c.table_splices.iter().filter(|s| s.block == block.id) {
                                // Same row IDs: preserve height until the changed
                                // row is measured. Structural changes replay in order.
                                if t.rows[splice.range.clone()] != splice.inserted {
                                    work.indexed_rows += t
                                        .heights
                                        .splice(splice.range.clone(), splice.inserted.len());
                                    t.rows.splice(
                                        splice.range.clone(),
                                        splice.inserted.iter().copied(),
                                    );
                                }
                            }
                        }
                        if let Some(rows) = dirty_rows.get(&block.id) {
                            for &id in rows {
                                if let Some(i) = model.row_index(id) {
                                    let h = self.measure_row(
                                        &model.rows[i],
                                        i == 0,
                                        &t,
                                        text,
                                        faces,
                                        theme,
                                        size,
                                        &mut work,
                                    );
                                    t.heights.set(i, h);
                                }
                            }
                        }
                    }
                    debug_assert_eq!(t.rows.len(), model.rows.len());
                    let h = t.heights.total();
                    (LayoutContent::Table(t), h)
                }
                Content::Text { kind, element } => {
                    let (scale, flags, x, quote, marker) = match kind {
                        TextKind::Heading(n) => (
                            match n {
                                1 => 1.75,
                                2 => 1.4,
                                3 => 1.18,
                                _ => 1.05,
                            },
                            inline::STRONG,
                            0.,
                            false,
                            None,
                        ),
                        TextKind::Quote(n) => {
                            (1., 0, (*n as f32 * 16.).min(width / 3.), true, None)
                        }
                        TextKind::List { depth, marker } => (
                            1.,
                            0,
                            (22. + *depth as f32 * 16.).min(width / 2.),
                            false,
                            Some(marker.clone()),
                        ),
                        _ => (1., 0, 0., false, None),
                    };
                    let h = self.ensure(
                        element,
                        text,
                        faces,
                        theme,
                        size * scale,
                        (width - x).max(size),
                        Align::Left,
                        flags,
                        &mut work,
                    );
                    (
                        LayoutContent::Parts {
                            items: vec![Part {
                                id: element.id,
                                x,
                                y: 0.,
                            }],
                            quote,
                            code: false,
                            marker,
                        },
                        h,
                    )
                }
                Content::Code { lines, .. } => {
                    let mut items = Vec::new();
                    let mut y = 12.;
                    for e in lines {
                        let h = self.ensure(
                            e,
                            text,
                            faces,
                            theme,
                            size * 0.9,
                            width - 24.,
                            Align::Left,
                            inline::CODE,
                            &mut work,
                        );
                        items.push(Part {
                            id: e.id,
                            x: 12.,
                            y,
                        });
                        y += h;
                    }
                    (
                        LayoutContent::Parts {
                            items,
                            quote: false,
                            code: true,
                            marker: None,
                        },
                        y.max(size * 1.25) + 12.,
                    )
                }
                Content::Rule => (LayoutContent::Rule, 12.),
            };
            self.blocks.insert(
                block.id,
                BlockCache {
                    y: 0.,
                    height,
                    content,
                },
            );
        }
        self.order = doc.blocks().iter().map(|b| b.id).collect();
        let mut y = 0.;
        self.width = width;
        for id in &self.order {
            let b = self.blocks.get_mut(id).unwrap();
            b.y = y;
            y += b.height + size * 0.8;
            if let LayoutContent::Table(t) = &b.content {
                self.width = self.width.max(t.width);
            }
        }
        self.height = y;
        self.revision = Some(doc.revision());
        self.config = Some(config);
        self.last_work = work;
        work
    }
    pub fn block_y(&self, id: Id) -> Option<f32> {
        self.blocks.get(&id).map(|b| b.y)
    }
    /// Only visible table rows are traversed. Draw clips are shared by the
    /// viewport, so many cells remain one batch/segment rather than N draw calls.
    pub fn scene(
        &mut self,
        text: &mut TextService,
        doc: &Document,
        viewport: Rect,
        scroll: Vec2,
    ) -> Scene {
        let mut scene = Scene::default();
        let (_, theme, width, size) = self.config.unwrap();
        let mut placed = Vec::new();
        let mut marker_draws = Vec::new();
        let origin = Vec2::new(viewport.x - scroll.x, viewport.y - scroll.y);
        let deco = |x, y, w, h, color| Decoration {
            rect: Rect::new(origin.x + x, origin.y + y, w, h),
            color,
        };
        for block in doc.blocks() {
            let id = block.id;
            let b = &self.blocks[&id];
            if b.y + b.height < scroll.y || b.y > scroll.y + viewport.height {
                continue;
            }
            match &b.content {
                LayoutContent::Parts {
                    items,
                    quote,
                    code,
                    marker,
                } => {
                    if *quote {
                        scene
                            .under
                            .push(deco(0., b.y, 3., b.height, theme.accent()));
                    }
                    if *code {
                        scene
                            .under
                            .push(deco(0., b.y, width, b.height, theme.panel()));
                    }
                    if let Some(marker) = marker {
                        let at = Vec2::new(origin.x + items[0].x - 22., origin.y + b.y);
                        marker_draws.push((marker.clone(), at));
                    }
                    for p in items {
                        let c = &self.texts[&p.id];
                        if b.y + p.y + c.height >= scroll.y
                            && b.y + p.y <= scroll.y + viewport.height
                        {
                            placed.push((p.id, Vec2::new(origin.x + p.x, origin.y + b.y + p.y)));
                        }
                    }
                }
                LayoutContent::Table(t) => {
                    let Content::Table(model) = &block.content else {
                        unreachable!()
                    };
                    let mut row = t.heights.row_at((scroll.y - b.y).max(0.));
                    let cw = t.width / t.align.len() as f32;
                    while row < t.rows.len() {
                        let y = b.y + t.heights.prefix(row);
                        if y > scroll.y + viewport.height {
                            break;
                        }
                        let h = t.heights.values[row];
                        scene.under.push(deco(
                            0.,
                            y,
                            t.width,
                            h,
                            if row == 0 {
                                Color([0.045, 0.080, 0.115, 1.])
                            } else if row % 2 == 0 {
                                theme.panel()
                            } else {
                                Color([0.017, 0.023, 0.032, 1.])
                            },
                        ));
                        scene.under.push(deco(0., y, t.width, 1., theme.grid()));
                        for col in 0..t.align.len() {
                            scene
                                .under
                                .push(deco(col as f32 * cw, y, 1., h, theme.grid()));
                            placed.push((
                                model.rows[row].cells[col].id,
                                Vec2::new(origin.x + col as f32 * cw + 10., origin.y + y + 8.),
                            ));
                        }
                        scene.under.push(deco(t.width - 1., y, 1., h, theme.grid()));
                        row += 1;
                    }
                    scene
                        .under
                        .push(deco(0., b.y + b.height - 1., t.width, 1., theme.grid()));
                }
                LayoutContent::Rule => {
                    scene
                        .under
                        .push(deco(0., b.y + 5., width, 1., theme.grid()))
                }
            }
        }
        for (id, at) in placed {
            let key = self.key(id);
            let c = self.texts.get_mut(&id).unwrap();
            if text.measure(c.handle).line_count() == 0 {
                c.shape(text, key, &mut Work::default());
            }
            scene.draws.push(Draw {
                block: c.handle,
                at,
                size: c.size,
                color: c.color,
                paint: c.paint,
                clip: Some(viewport),
            });
            for r in &c.rich.runs {
                let flags = r.flags | c.flags;
                if flags & (inline::CODE | inline::STRIKE | inline::LINK) == 0 {
                    continue;
                }
                for span in text.measure(c.handle).selection(r.range.clone()) {
                    let x = at.x + span.x_em * c.size;
                    let y = at.y + span.y_em * c.size;
                    let w = span.width_em * c.size;
                    if flags & inline::CODE != 0 && c.flags & inline::CODE == 0 {
                        scene.under.push(Decoration {
                            rect: Rect::new(x - 2., y, w + 4., span.height_em * c.size),
                            color: theme.panel(),
                        });
                    }
                    if flags & inline::STRIKE != 0 {
                        scene.over.push(Decoration {
                            rect: Rect::new(x, y + c.size * 0.6, w, 1.),
                            color: c.color,
                        });
                    }
                    if flags & inline::LINK != 0 {
                        scene.over.push(Decoration {
                            rect: Rect::new(x, y + c.size * 1.05, w, 1.),
                            color: theme.accent(),
                        });
                    }
                }
            }
            scene.placed.push((id, at));
        }
        // Shape every marker before any prepare/draw can bind atlas textures.
        for (marker, at) in marker_draws {
            let style = Style {
                chain: self.config.unwrap().0.prose[0],
                wrap_em: None,
                align: Align::Left,
                line_spacing: 1.25,
            };
            let block = text.shape_transient(&marker, &style).unwrap();
            scene.draws.push(Draw {
                block,
                at,
                size,
                color: theme.accent(),
                clip: Some(viewport),
                ..Default::default()
            });
        }
        scene
    }
    /// Projected positions map through explicit source spans (entities, escaped
    /// delimiters and removed markup are not a constant-offset subtraction).
    pub fn hit_source(
        &self,
        scene: &Scene,
        point: Vec2,
        text: &TextService,
        doc: &Document,
    ) -> Option<usize> {
        for &(id, at) in &scene.placed {
            let c = &self.texts[&id];
            if point.y < at.y
                || point.y > at.y + c.height
                || point.x < at.x
                || point.x > at.x + c.style.wrap_em.unwrap_or(100.) * c.size
            {
                continue;
            }
            let hit = text.measure(c.handle).hit_test(Vec2::new(
                (point.x - at.x) / c.size,
                (point.y - at.y) / c.size,
            ))?;
            let raw = c.rich.source_byte(hit.byte_index);
            let i = c
                .origins
                .partition_point(|l| l.offset <= raw)
                .saturating_sub(1);
            let line = &c.origins[i];
            return doc.resolve(Origin {
                line: line.origin.line,
                column: line.origin.column + raw - line.offset,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heights_append_update_splice_and_search() {
        let mut h = Heights::from(vec![10., 20., 30.]);
        assert_eq!(h.total(), 60.);
        assert_eq!(h.row_at(10.), 1);
        h.push(40.);
        assert_eq!(h.prefix(4), 100.);
        h.set(1, 25.);
        assert_eq!(h.total(), 105.);
        h.splice(1..2, 2);
        assert_eq!(h.values, vec![10., 0., 0., 30., 40.]);
        h.set(1, 4.);
        h.set(2, 6.);
        assert_eq!(h.total(), 90.);
    }
    fn setup() -> (TextService, Faces) {
        let mut text = TextService::new();
        let faces = crate::fonts::load(&mut text, None);
        (text, faces)
    }
    #[test]
    fn color_theme_never_parses_or_shapes_and_tables_update_one_row() {
        let (mut text, faces) = setup();
        let mut doc = Document::new(
            "# Hi\n\n**bold _both_** and `code` &amp; [link](url)\n\n| A | B |\n| --- | ---: |\n| x | **y** |\n",
        );
        let mut view = Preview::new(300);
        view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
        let revision = doc.revision();
        #[cfg(feature = "perf-counters")]
        sanscale::profiling::reset_work_counters();
        let theme = Theme {
            alternate: true,
            ..Default::default()
        };
        let w = view.sync(&doc, &mut text, faces, theme, 500., 17.);
        assert_eq!(w.layout_requests, 0);
        assert_eq!(doc.revision(), revision);
        #[cfg(feature = "perf-counters")]
        {
            let c = sanscale::profiling::work_counters();
            assert_eq!((c.shape_calls, c.flow_calls, c.source_reads), (0, 0, 0));
        }
        doc.append("| new | a much longer streamed cell that wraps over several lines without changing column widths |\n").unwrap();
        let w = view.sync(&doc, &mut text, faces, theme, 500., 17.);
        assert_eq!(w.measured_rows, 1);
        assert_eq!(w.layout_requests, 2);
        assert!(w.indexed_rows <= 1);
        view.release(&mut text);
    }
    #[test]
    fn missed_history_and_multiple_edits_reconcile_without_losing_rows() {
        let (mut text, faces) = setup();
        let mut doc = Document::new("| A | B |\n| --- | --- |\n");
        let mut view = Preview::new(301);
        view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
        for _ in 0..4 {
            doc.append("| x | y |\n").unwrap();
        }
        view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
        for _ in 0..70 {
            doc.append("| xx | yy |\n").unwrap();
        }
        view.sync(&doc, &mut text, faces, Theme::default(), 400., 17.);
        let scene = view.scene(
            &mut text,
            &doc,
            Rect::new(0., 0., 400., 200.),
            Vec2::new(0., 0.),
        );
        assert!(!scene.draws.is_empty());
        assert!(scene.draws.len() < 20);
        view.release(&mut text);
    }
    #[test]
    fn changed_row_height_moves_following_blocks_without_reshaping_them() {
        let (mut text, faces) = setup();
        let mut doc =
            Document::new("| A | B |\n| --- | --- |\n| short | value |\n\nA following paragraph.");
        let mut view = Preview::new(302);
        view.sync(&doc, &mut text, faces, Theme::default(), 320., 17.);
        let after = doc.blocks()[1].id;
        let old_y = view.block_y(after).unwrap();
        let old_handle = view.texts[&after].handle;
        let at = doc.source().to_string().find("value").unwrap();
        doc.edit(
            at..at + 5,
            "a much longer cell that takes several visual lines to display",
        )
        .unwrap();
        let w = view.sync(&doc, &mut text, faces, Theme::default(), 320., 17.);
        assert_eq!(w.layout_requests, 1);
        assert_eq!(w.measured_rows, 1);
        assert!(view.block_y(after).unwrap() > old_y);
        assert_eq!(view.texts[&after].handle, old_handle);
        view.release(&mut text);
    }
    #[test]
    fn projected_clicks_map_entities_and_shifted_cells_back_to_source() {
        let (mut text, faces) = setup();
        let mut doc = Document::new("| A | B |\n| --- | --- |\n| x | &amp; café |\n");
        let mut view = Preview::new(303);
        for value in ["x", "much longer"] {
            if value != "x" {
                let i = doc.source().to_string().find("| x |").unwrap() + 2;
                doc.edit(i..i + 1, value).unwrap();
            }
            view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
            let scene = view.scene(
                &mut text,
                &doc,
                Rect::new(10., 20., 500., 300.),
                Vec2::new(0., 0.),
            );
            let Content::Table(t) = &doc.blocks()[0].content else {
                panic!()
            };
            let id = t.rows[1].cells[1].id;
            let c = &view.texts[&id];
            let at = scene.placed.iter().find(|p| p.0 == id).unwrap().1;
            let layout = text.measure(c.handle);
            let caret = layout.caret_rect(layout.caret_at(0));
            let point = Vec2::new(
                at.x + caret.x_em * c.size,
                at.y + (caret.y_em + caret.height_em * 0.5) * c.size,
            );
            assert_eq!(
                view.hit_source(&scene, point, &text, &doc),
                doc.source().to_string().find("&amp;")
            );
        }
        view.release(&mut text);
    }
    #[test]
    fn reusing_a_view_for_another_document_cannot_alias_old_keys() {
        let (mut text, faces) = setup();
        let a = Document::new("old **content**");
        let b = Document::new("new _different words_");
        let mut view = Preview::new(304);
        view.sync(&a, &mut text, faces, Theme::default(), 400., 17.);
        view.sync(&b, &mut text, faces, Theme::default(), 400., 17.);
        let c = &view.texts[&b.blocks()[0].id];
        assert_eq!(
            text.measure(c.handle).len_bytes(),
            "new different words".len()
        );
        view.release(&mut text);
    }
    #[test]
    fn edited_layout_matches_a_fresh_view_including_table_structure() {
        let (mut text, faces) = setup();
        let mut doc = Document::new(
            "# Header\n\n| A | B |\n| --- | --- |\n| first | **second** |\n| third | fourth |\n\nAfter the table.\n",
        );
        let mut view = Preview::new(305);
        let mut seed = 73u64;
        for step in 0..100 {
            view.sync(&doc, &mut text, faces, Theme::default(), 380., 17.);
            let mut cold = Preview::new(10000 + step);
            cold.sync(&doc, &mut text, faces, Theme::default(), 380., 17.);
            assert!((view.height - cold.height).abs() < 0.02);
            for block in doc.blocks() {
                let a = &view.blocks[&block.id];
                let b = &cold.blocks[&block.id];
                assert!((a.y - b.y).abs() < 0.02);
                assert!((a.height - b.height).abs() < 0.02);
            }
            cold.release(&mut text);
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let s = doc.source().to_string();
            let points = s
                .char_indices()
                .map(|(i, _)| i)
                .chain([s.len()])
                .collect::<Vec<_>>();
            let a = seed as usize % points.len();
            let z = (a + ((seed >> 24) as usize) % 4).min(points.len() - 1);
            doc.edit(
                points[a]..points[z],
                [" | ", "\n", "**", "é", "", ":---:"][(seed >> 32) as usize % 6],
            )
            .unwrap();
        }
        view.release(&mut text);
    }
    #[test]
    fn combined_faces_grapheme_boundaries_and_real_hard_breaks() {
        let (mut text, faces) = setup();
        let doc = Document::new("***both*** **a**\u{301}  \nnext");
        let mut view = Preview::new(306);
        view.sync(&doc, &mut text, faces, Theme::default(), 500., 17.);
        let c = &view.texts[&doc.blocks()[0].id];
        assert_eq!(c.rich.text, "both a\u{301}\nnext");
        assert_eq!(
            c.fonts,
            vec![
                FontSpan {
                    range: 0..4,
                    chain: faces.prose[3]
                },
                FontSpan {
                    range: 5..8,
                    chain: faces.prose[1]
                }
            ]
        );
        assert_eq!(text.measure(c.handle).line_count(), 2);
        assert_eq!(text.measure(c.handle).len_bytes(), c.rich.text.len());
        let w = view.sync(
            &doc,
            &mut text,
            faces,
            Theme {
                italic: false,
                ..Default::default()
            },
            500.,
            17.,
        );
        assert_eq!(w.layout_requests, 1);
        assert_eq!(
            view.texts[&doc.blocks()[0].id].fonts[0].chain,
            faces.prose[1]
        );
        view.release(&mut text);
    }
}
