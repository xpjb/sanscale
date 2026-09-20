//! Experimental, window-independent Markdown engine. No dependency on sanscale:
//! consumers can retain blocks/cells, append streamed text, or apply UTF-8 edits.
//! See README.md beside this module for the supported dialect and work bounds.
pub mod inline;
mod parse;
#[cfg(test)]
mod tests;

use inline::RichText;
use parse::{Class, Kind, Mode};
use ropey::Rope;
use std::sync::Arc;
use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
};
pub type Id = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    Left,
    Center,
    Right,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextKind {
    Paragraph,
    Heading(u8),
    Quote(u8),
    List { depth: u8, marker: String },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origin {
    pub line: Id,
    pub column: usize,
}
#[derive(Clone, Debug)]
pub struct RawLine {
    pub offset: usize,
    pub origin: Origin,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Projection {
    Prose,
    Cell,
    Code,
}
#[derive(Clone, Debug)]
pub struct Element {
    pub id: Id,
    /// Only projected text/inline roles change this revision, not source shifts.
    pub revision: u64,
    pub rich: Arc<RichText>,
    raw: String,
    projection: Projection,
    pub origins: Vec<RawLine>,
}
impl Element {
    pub fn origin_at(&self, display: usize) -> Origin {
        let byte = self.rich.source_byte(display);
        let n = self
            .origins
            .partition_point(|l| l.offset <= byte)
            .saturating_sub(1);
        let line = &self.origins[n];
        Origin {
            line: line.origin.line,
            column: line.origin.column + byte.saturating_sub(line.offset),
        }
    }
}
#[derive(Clone, Debug)]
pub struct Row {
    pub id: Id,
    pub cells: Vec<Element>,
}
#[derive(Clone, Debug)]
pub struct Table {
    pub align: Vec<Alignment>,
    pub rows: Vec<Row>,
    index: HashMap<Id, usize>,
}
impl Table {
    pub fn row_index(&self, id: Id) -> Option<usize> {
        self.index.get(&id).copied()
    }
}
#[derive(Clone, Debug)]
pub enum Content {
    Text {
        kind: TextKind,
        element: Element,
    },
    Code {
        language: String,
        closed: bool,
        lines: Vec<Element>,
    },
    Table(Table),
    Rule,
}
#[derive(Clone, Debug)]
pub struct Block {
    pub id: Id,
    pub revision: u64,
    pub lines: Range<usize>,
    pub content: Content,
}
impl Block {
    pub fn elements(&self) -> Box<dyn Iterator<Item = &Element> + '_> {
        match &self.content {
            Content::Text { element, .. } => Box::new(std::iter::once(element)),
            Content::Code { lines, .. } => Box::new(lines.iter()),
            Content::Table(t) => Box::new(t.rows.iter().flat_map(|r| r.cells.iter())),
            Content::Rule => Box::new(std::iter::empty()),
        }
    }
}
/// Actual adapter work, not elapsed-time guesses or a single 'cache hit' count.
#[derive(Clone, Copy, Debug, Default)]
pub struct Work {
    pub classified_lines: usize,
    pub projected_elements: usize,
    pub projected_bytes: usize,
    pub reused_elements: usize,
    pub metadata_lines: usize,
}
#[derive(Clone, Debug)]
pub struct TableSplice {
    pub block: Id,
    pub range: Range<usize>,
    pub inserted: Vec<Id>,
}
#[derive(Clone, Debug, Default)]
pub struct Change {
    pub revision: u64,
    pub changed_blocks: Vec<Id>,
    pub removed_elements: Vec<Id>,
    pub table_splices: Vec<TableSplice>,
    pub dirty_rows: Vec<(Id, Id)>,
    /// The renderer must reconcile this block's structure, not replay a splice.
    pub reconcile: Vec<Id>,
    pub work: Work,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditError {
    InvalidUtf8Range,
    InvalidUtf8Stream,
    IncompleteUtf8Stream,
}
impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for EditError {}
struct Line {
    id: Id,
    version: u32,
    raw: String,
    valid: bool,
    incoming: Mode,
    outgoing: Mode,
    class: Class,
}
impl Line {
    fn new(id: Id, version: u32, raw: String) -> Self {
        Self {
            id,
            version,
            raw,
            valid: false,
            incoming: Mode::Idle,
            outgoing: Mode::Idle,
            class: Class::blank(),
        }
    }
}

static DOCUMENT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
pub struct Document {
    identity: u64,
    source: Rope,
    lines: Vec<Line>,
    line_index: HashMap<Id, usize>,
    blocks: Vec<Block>,
    revision: u64,
    next_id: Id,
    history: VecDeque<Change>,
    last_change: Change,
}
impl Default for Document {
    fn default() -> Self {
        Self::new("")
    }
}
impl Document {
    /// Input is preserved byte-for-byte. LF is the physical line separator;
    /// UI/transport adapters may normalize CRLF before calling this API.
    pub fn new(source: &str) -> Self {
        let mut d = Self {
            identity: DOCUMENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            source: Rope::from_str(source),
            lines: Vec::new(),
            line_index: HashMap::new(),
            blocks: Vec::new(),
            revision: 0,
            next_id: 1,
            history: VecDeque::new(),
            last_change: Change::default(),
        };
        for i in 0..d.source.len_lines() {
            let id = d.alloc();
            let raw = d.source_line(i);
            d.lines.push(Line::new(id, 0, raw));
            d.line_index.insert(id, i);
        }
        let mut change = Change::default();
        d.reclassify(0, d.lines.len() - 1, &mut change.work);
        d.rebuild(0..0, 0..d.lines.len(), &mut change);
        d.last_change = change;
        d
    }
    fn alloc(&mut self) -> Id {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("Markdown document identity capacity");
        id
    }
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn last_change(&self) -> &Change {
        &self.last_change
    }
    pub fn identity(&self) -> u64 {
        self.identity
    }
    pub fn source(&self) -> &Rope {
        &self.source
    }
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
    pub fn line_text(&self, i: usize) -> Option<&str> {
        self.lines.get(i).map(|l| l.raw.as_str())
    }
    pub fn line_key(&self, i: usize) -> Option<(Id, u32)> {
        self.lines.get(i).map(|l| (l.id, l.version))
    }
    pub fn resolve(&self, origin: Origin) -> Option<usize> {
        let &i = self.line_index.get(&origin.line)?;
        let raw = &self.lines[i].raw;
        let column = origin.column.min(raw.len());
        let mut column = column;
        while !raw.is_char_boundary(column) {
            column -= 1;
        }
        Some(self.source.line_to_byte(i) + column)
    }
    pub fn block_at_source(&self, byte: usize) -> Option<Id> {
        let line = self.source.byte_to_line(byte.min(self.source.len_bytes()));
        self.lines[line].class.owner
    }
    pub fn changes_since(&self, revision: u64) -> Option<impl Iterator<Item = &Change>> {
        if revision > self.revision
            || self
                .history
                .front()
                .is_some_and(|c| revision + 1 < c.revision)
        {
            return None;
        }
        Some(self.history.iter().filter(move |c| c.revision > revision))
    }
    pub fn append(&mut self, chunk: &str) -> Result<&Change, EditError> {
        let end = self.source.len_bytes();
        self.edit(end..end, chunk)
    }
    pub fn edit(&mut self, range: Range<usize>, insert: &str) -> Result<&Change, EditError> {
        if range.start > range.end
            || range.end > self.source.len_bytes()
            || [range.start, range.end]
                .into_iter()
                .any(|b| self.source.char_to_byte(self.source.byte_to_char(b)) != b)
        {
            return Err(EditError::InvalidUtf8Range);
        }
        if self.source.byte_slice(range.clone()) == insert {
            self.last_change = Change {
                revision: self.revision,
                ..Default::default()
            };
            return Ok(&self.last_change);
        }
        let first = self.source.byte_to_line(range.start);
        let last = self.source.byte_to_line(range.end);
        let restart = first.saturating_sub(1);
        let mut old_first = self.blocks.partition_point(|b| b.lines.end <= restart);
        // Losing a heading/table opener can merge into the preceding paragraph.
        // Include that boundary block even when restart is exactly its end.
        if old_first > 0 && self.blocks[old_first - 1].lines.end == restart {
            old_first -= 1;
        }
        let region_start = self
            .blocks
            .get(old_first)
            .map_or(restart, |b| b.lines.start.min(restart));
        let keep = self.lines[first].id;
        let version = self.lines[first].version.wrapping_add(1);
        for l in &self.lines[first..=last] {
            self.line_index.remove(&l.id);
        }
        let chars = self.source.byte_to_char(range.start)..self.source.byte_to_char(range.end);
        self.source.remove(chars);
        self.source
            .insert(self.source.byte_to_char(range.start), insert);
        let new_last = self.source.byte_to_line(range.start + insert.len());
        let mut replacement = Vec::new();
        for i in first..=new_last {
            let id = if i == first { keep } else { self.alloc() };
            replacement.push(Line::new(
                id,
                if i == first { version } else { 0 },
                self.source_line(i),
            ));
        }
        self.lines.splice(first..=last, replacement);
        // Appends update only the tail. Middle insertions/deletions move suffix
        // line indices; that metadata work is separate from parsing/projection.
        let mut change = Change {
            revision: self.revision + 1,
            ..Default::default()
        };
        let index_end = if new_last == last {
            new_last + 1
        } else {
            self.lines.len()
        };
        for i in first..index_end {
            self.line_index.insert(self.lines[i].id, i);
            change.work.metadata_lines += 1;
        }
        let stop = self.reclassify(restart, new_last, &mut change.work);
        let delta = new_last as isize - last as isize;
        let old_stop = (stop as isize - delta) as usize;
        let old_end = self.blocks.partition_point(|b| b.lines.start < old_stop);
        let region_end = self
            .blocks
            .get(old_end.wrapping_sub(1))
            .filter(|_| old_end > old_first)
            .map_or(stop, |b| stop.max((b.lines.end as isize + delta) as usize))
            .min(self.lines.len());
        self.revision += 1;
        let patched = old_end == old_first + 1
            && (self.patch_table(
                old_first,
                first..last + 1,
                first..new_last + 1,
                region_start..region_end,
                stop,
                &mut change,
            ) || self.patch_code(
                old_first,
                first..last + 1,
                first..new_last + 1,
                region_start..region_end,
                stop,
                &mut change,
            ));
        if !patched {
            self.rebuild(old_first..old_end, region_start..region_end, &mut change);
        }
        // Locate the unaffected suffix using source lines rather than changed
        // block counts. Structural middle edits necessarily move this metadata.
        if delta != 0 {
            let changed = change
                .changed_blocks
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();
            for b in self
                .blocks
                .iter_mut()
                .rev()
                .take_while(|b| !changed.contains(&b.id) && b.lines.start >= old_stop)
            {
                b.lines.start = (b.lines.start as isize + delta) as usize;
                b.lines.end = (b.lines.end as isize + delta) as usize;
                change.work.metadata_lines += 1;
            }
        }
        self.history.push_back(change.clone());
        if self.history.len() > 64 {
            self.history.pop_front();
        }
        self.last_change = change;
        Ok(&self.last_change)
    }
    fn source_line(&self, i: usize) -> String {
        let mut s = self.source.line(i).to_string();
        if s.ends_with('\n') {
            s.pop();
        }
        s
    }
    fn reclassify(&mut self, start: usize, forced_last: usize, work: &mut Work) -> usize {
        let mut mode = if start == 0 {
            Mode::Idle
        } else {
            self.lines[start - 1].outgoing.clone()
        };
        let mut i = start;
        while i < self.lines.len() {
            if i > forced_last && self.lines[i].valid && self.lines[i].incoming == mode {
                break;
            }
            let (class, next) = parse::classify(
                self.lines[i].id,
                &self.lines[i].raw,
                self.lines.get(i + 1).map(|l| l.raw.as_str()),
                &mode,
            );
            let line = &mut self.lines[i];
            line.incoming = mode;
            line.outgoing = next.clone();
            line.class = class;
            line.valid = true;
            mode = next;
            i += 1;
            work.classified_lines += 1;
        }
        i
    }
    fn make_element(
        &mut self,
        id: Id,
        old: Option<Element>,
        raw: String,
        origins: Vec<RawLine>,
        literal: bool,
        cell: bool,
        work: &mut Work,
    ) -> Element {
        let projection = if literal {
            Projection::Code
        } else if cell {
            Projection::Cell
        } else {
            Projection::Prose
        };
        if let Some(mut old) = old {
            if old.raw == raw && old.projection == projection {
                old.origins = origins;
                work.reused_elements += 1;
                return old;
            }
            work.projected_elements += 1;
            work.projected_bytes += raw.len();
            let rich = if literal {
                RichText::literal(&raw)
            } else if cell {
                inline::parse_cell(&raw)
            } else {
                inline::parse(&raw)
            };
            if !rich.same_pixels(&old.rich) {
                old.revision += 1;
            }
            old.rich = Arc::new(rich);
            old.raw = raw;
            old.projection = projection;
            old.origins = origins;
            return old;
        }
        work.projected_elements += 1;
        work.projected_bytes += raw.len();
        let rich = if literal {
            RichText::literal(&raw)
        } else if cell {
            inline::parse_cell(&raw)
        } else {
            inline::parse(&raw)
        };
        Element {
            id,
            revision: self.revision,
            rich: Arc::new(rich),
            raw,
            projection,
            origins,
        }
    }
    fn make_row(&mut self, line: usize, columns: usize, old: Option<Row>, work: &mut Work) -> Row {
        let id = self.lines[line].id;
        let raw = self.lines[line].raw.clone();
        let parts = parse::cells(&raw);
        let mut old = old.map_or_else(Vec::new, |r| r.cells).into_iter();
        let mut cells = Vec::new();
        for col in 0..columns {
            let previous = old.next();
            let eid = previous.as_ref().map_or_else(|| self.alloc(), |c| c.id);
            let part = parts.get(col).cloned().unwrap_or(raw.len()..raw.len());
            cells.push(self.make_element(
                eid,
                previous,
                raw[part.clone()].to_owned(),
                vec![RawLine {
                    offset: 0,
                    origin: Origin {
                        line: id,
                        column: part.start,
                    },
                }],
                false,
                true,
                work,
            ));
        }
        Row { id, cells }
    }
    /// The important streaming path: modify just the touched table rows/cells.
    /// No complete table snapshot, row scan, or re-projection of the prefix.
    fn patch_table(
        &mut self,
        index: usize,
        old_edit: Range<usize>,
        new_edit: Range<usize>,
        region: Range<usize>,
        stop: usize,
        change: &mut Change,
    ) -> bool {
        let b = &self.blocks[index];
        let Content::Table(t) = &b.content else {
            return false;
        };
        let Kind::TableHead(align) = &self.lines[region.start].class.kind else {
            return false;
        };
        if b.id != self.lines[region.start].id
            || *align != t.align
            || old_edit.start < b.lines.start + 2
            || old_edit.start > b.lines.end
        {
            return false;
        }
        let mut end = region.end;
        while end > region.start + 2 && self.lines[end - 1].class.owner.is_none() {
            end -= 1;
        }
        if self.lines[new_edit.start..stop.min(end)]
            .iter()
            .any(|l| l.class.owner != Some(b.id) || l.class.kind != Kind::TableRow)
        {
            return false;
        }
        let row_start = old_edit.start - b.lines.start - 1;
        let row_end = old_edit.end.min(b.lines.end) - b.lines.start - 1;
        let columns = t.align.len();
        let block_id = b.id;
        let Content::Table(t) = &mut self.blocks[index].content else {
            unreachable!()
        };
        let old_rows = t.rows.drain(row_start..row_end).collect::<Vec<_>>();
        for row in &old_rows {
            t.index.remove(&row.id);
        }
        let mut old: HashMap<_, _> = old_rows.into_iter().map(|r| (r.id, r)).collect();
        let mut rows = Vec::new();
        for line in new_edit.start..new_edit.end.min(end) {
            let id = self.lines[line].id;
            rows.push(self.make_row(line, columns, old.remove(&id), &mut change.work));
        }
        for row in old.into_values() {
            change
                .removed_elements
                .extend(row.cells.into_iter().map(|c| c.id));
        }
        let inserted = rows.iter().map(|r| r.id).collect::<Vec<_>>();
        change
            .dirty_rows
            .extend(inserted.iter().map(|&r| (block_id, r)));
        let b = &mut self.blocks[index];
        let Content::Table(t) = &mut b.content else {
            unreachable!()
        };
        let inserted_count = rows.len();
        t.rows.splice(row_start..row_start, rows);
        let update_end = if inserted_count == row_end - row_start {
            row_start + inserted_count
        } else {
            t.rows.len()
        };
        for i in row_start..update_end {
            t.index.insert(t.rows[i].id, i);
            change.work.metadata_lines += 1;
        }
        b.lines = region.start..end;
        b.revision = self.revision;
        change.table_splices.push(TableSplice {
            block: block_id,
            range: row_start..row_end,
            inserted,
        });
        change.changed_blocks.push(block_id);
        true
    }
    fn patch_code(
        &mut self,
        index: usize,
        old_edit: Range<usize>,
        new_edit: Range<usize>,
        region: Range<usize>,
        stop: usize,
        change: &mut Change,
    ) -> bool {
        let b = &self.blocks[index];
        let Content::Code {
            language,
            closed,
            lines,
        } = &b.content
        else {
            return false;
        };
        let Kind::FenceOpen {
            language: new_language,
        } = &self.lines[region.start].class.kind
        else {
            return false;
        };
        if b.id != self.lines[region.start].id
            || language != new_language
            || old_edit.start <= b.lines.start
            || old_edit.start > b.lines.end
        {
            return false;
        }
        let mut end = region.end;
        while end > region.start + 1 && self.lines[end - 1].class.owner.is_none() {
            end -= 1;
        }
        if self.lines[new_edit.start..stop.min(end)].iter().any(|l| {
            l.class.owner != Some(b.id)
                || !matches!(l.class.kind, Kind::FenceLine | Kind::FenceClose)
        }) {
            return false;
        }
        let was_closed = *closed;
        let is_closed = self.lines[end - 1].class.kind == Kind::FenceClose;
        let body_end = end - usize::from(is_closed);
        let old_body_end = b.lines.end - usize::from(was_closed);
        let start = old_edit.start.min(old_body_end) - b.lines.start - 1;
        let finish = old_edit.end.min(old_body_end) - b.lines.start - 1;
        if start > lines.len() || finish < start {
            return false;
        }
        let block_id = b.id;
        let Content::Code { lines, .. } = &mut self.blocks[index].content else {
            unreachable!()
        };
        let mut old = lines
            .drain(start..finish)
            .map(|e| (e.id, e))
            .collect::<HashMap<_, _>>();
        let mut replacement = Vec::new();
        for i in new_edit.start..new_edit.end.min(body_end) {
            let line = &self.lines[i];
            let id = line.id;
            let raw = line.raw[line.class.content.clone()].to_owned();
            let origin = Origin {
                line: id,
                column: line.class.content.start,
            };
            replacement.push(self.make_element(
                id,
                old.remove(&id),
                raw,
                vec![RawLine { offset: 0, origin }],
                true,
                false,
                &mut change.work,
            ));
            change.work.metadata_lines += 1;
        }
        change.removed_elements.extend(old.into_keys());
        let b = &mut self.blocks[index];
        let Content::Code { lines, closed, .. } = &mut b.content else {
            unreachable!()
        };
        lines.splice(start..start, replacement);
        *closed = is_closed;
        b.lines = region.start..end;
        b.revision = self.revision;
        change.changed_blocks.push(block_id);
        change.reconcile.push(block_id);
        true
    }
    fn rebuild(&mut self, old_range: Range<usize>, region: Range<usize>, change: &mut Change) {
        let mut old = self
            .blocks
            .drain(old_range.clone())
            .map(|b| (b.id, b))
            .collect::<HashMap<_, _>>();
        let mut rebuilt = Vec::new();
        let mut i = region.start;
        while i < region.end {
            change.work.metadata_lines += 1;
            let Some(owner) = self.lines[i].class.owner else {
                i += 1;
                continue;
            };
            let start = i;
            i += 1;
            while i < region.end && self.lines[i].class.owner == Some(owner) {
                i += 1;
                change.work.metadata_lines += 1;
            }
            let mut previous = old.remove(&owner);
            let mut old_elements = HashMap::new();
            if let Some(b) = previous.take() {
                match b.content {
                    Content::Text { element, .. } => {
                        old_elements.insert(element.id, element);
                    }
                    Content::Code { lines, .. } => {
                        old_elements.extend(lines.into_iter().map(|e| (e.id, e)))
                    }
                    Content::Table(t) => old_elements
                        .extend(t.rows.into_iter().flat_map(|r| r.cells).map(|e| (e.id, e))),
                    Content::Rule => {}
                }
            }
            let kind = self.lines[start].class.kind.clone();
            // Row cells keep identities by (stable physical line, column), not
            // by absolute source offsets or a fresh ID on every parse.
            let mut old_cells: HashMap<(Id, usize), Element> = HashMap::new();
            if matches!(kind, Kind::TableHead(_)) {
                let mut grouped: HashMap<Id, Vec<Element>> = HashMap::new();
                for e in old_elements.drain().map(|(_, e)| e) {
                    grouped.entry(e.origins[0].origin.line).or_default().push(e);
                }
                for (id, mut cells) in grouped {
                    cells.sort_by_key(|e| (e.origins[0].origin.column, e.id));
                    for (n, c) in cells.into_iter().enumerate() {
                        old_cells.insert((id, n), c);
                    }
                }
            }
            let content = match kind {
                Kind::Text(kind) => {
                    let mut raw = String::new();
                    let mut origins = Vec::new();
                    for n in start..i {
                        if !matches!(self.lines[n].class.kind, Kind::Text(_)) {
                            continue;
                        }
                        if !raw.is_empty() {
                            raw.push('\n');
                        }
                        let l = &self.lines[n];
                        origins.push(RawLine {
                            offset: raw.len(),
                            origin: Origin {
                                line: l.id,
                                column: l.class.content.start,
                            },
                        });
                        raw.push_str(&l.raw[l.class.content.clone()]);
                    }
                    let e = self.make_element(
                        owner,
                        old_elements.remove(&owner),
                        raw,
                        origins,
                        false,
                        false,
                        &mut change.work,
                    );
                    Content::Text { kind, element: e }
                }
                Kind::FenceOpen { language, .. } => {
                    let mut lines = Vec::new();
                    let closed = matches!(self.lines[i - 1].class.kind, Kind::FenceClose);
                    for n in start + 1..i {
                        if self.lines[n].class.kind != Kind::FenceLine {
                            continue;
                        }
                        let l = &self.lines[n];
                        let id = l.id;
                        let raw = l.raw[l.class.content.clone()].to_owned();
                        let origin = Origin {
                            line: id,
                            column: l.class.content.start,
                        };
                        lines.push(self.make_element(
                            id,
                            old_elements.remove(&id),
                            raw,
                            vec![RawLine { offset: 0, origin }],
                            true,
                            false,
                            &mut change.work,
                        ));
                    }
                    Content::Code {
                        language,
                        closed,
                        lines,
                    }
                }
                Kind::TableHead(align) => {
                    let mut rows = Vec::new();
                    let mut index = HashMap::new();
                    for n in (start..i).filter(|&n| n != start + 1) {
                        let id = self.lines[n].id;
                        let cells = (0..align.len())
                            .filter_map(|col| old_cells.remove(&(id, col)))
                            .collect::<Vec<_>>();
                        let row = self.make_row(
                            n,
                            align.len(),
                            (!cells.is_empty()).then_some(Row { id, cells }),
                            &mut change.work,
                        );
                        change.dirty_rows.push((owner, id));
                        index.insert(id, rows.len());
                        rows.push(row);
                    }
                    Content::Table(Table { align, rows, index })
                }
                Kind::Rule => Content::Rule,
                _ => unreachable!("group must start with an opening line: {kind:?}"),
            };
            change.removed_elements.extend(old_elements.into_keys());
            change
                .removed_elements
                .extend(old_cells.into_values().map(|e| e.id));
            change.changed_blocks.push(owner);
            change.reconcile.push(owner);
            rebuilt.push(Block {
                id: owner,
                revision: self.revision,
                lines: start..i,
                content,
            });
        }
        for block in old.into_values() {
            change
                .removed_elements
                .extend(block.elements().map(|e| e.id));
        }
        let live = rebuilt
            .iter()
            .flat_map(|b| b.elements().map(|e| e.id))
            .collect::<std::collections::HashSet<_>>();
        change.removed_elements.retain(|id| !live.contains(id));
        self.blocks
            .splice(old_range.start..old_range.start, rebuilt);
    }
}

/// Byte-stream adapter: UTF-8 may be split across transport chunks. An incomplete
/// suffix is buffered, not replaced with U+FFFD. Invalid input is rejected before
/// mutating the document; finish rejects a truncated scalar.
#[derive(Default)]
pub struct Stream {
    pending: Vec<u8>,
}
impl Stream {
    pub fn push(&mut self, doc: &mut Document, bytes: &[u8]) -> Result<(), EditError> {
        let mut joined = self.pending.clone();
        joined.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&joined) {
            Ok(_) => joined.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => return Err(EditError::InvalidUtf8Stream),
        };
        doc.append(std::str::from_utf8(&joined[..valid]).unwrap())?;
        self.pending = joined[valid..].to_vec();
        Ok(())
    }
    pub fn finish(&self) -> Result<(), EditError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(EditError::IncompleteUtf8Stream)
        }
    }
}
