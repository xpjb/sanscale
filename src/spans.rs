//! Inline font inputs and immutable foreground-paint snapshots.

use crate::text::{Color, FontChainHandle};
use std::{num::NonZeroU64, ops::Range};
use unicode_segmentation::UnicodeSegmentation;

/// A paragraph-local UTF-8 byte range choosing an ordered font fallback chain.
///
/// Ranges must be nonempty, sorted, nonoverlapping, and begin/end at extended grapheme
/// boundaries. Gaps inherit `Style.chain`. Changing effective font spans requires
/// a new `ParagraphKey::generation`; colors do not. Line metrics stay those of
/// the base style, even when an inline face is taller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontSpan {
    pub range: Range<usize>,
    pub chain: FontChainHandle,
}

/// A block-local byte range overriding the draw's foreground color.
///
/// Sorted, nonoverlapping, nonempty ranges; gaps use `Draw.color`. The color at
/// a shaped cluster's starting byte colors the whole cluster. Boundaries never
/// split shaping, and may fall inside a ligature or grapheme. Out-of-block ranges
/// simply color no glyphs. Native-color emoji are not tinted.
#[derive(Clone, Debug, PartialEq)]
pub struct PaintSpan {
    pub range: Range<usize>,
    pub color: Color,
}

/// An immutable paint snapshot in one `TextService`'s pool. Release explicitly
/// with `drop_paint`. Slot reuse cannot revive a stale handle. Like other service
/// handles, this is local to its originating service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaintHandle(NonZeroU64);

impl PaintHandle {
    fn new(slot: usize, generation: u32) -> Self {
        Self(NonZeroU64::new((u64::from(generation) << 32) | slot as u64).unwrap())
    }
    fn slot(self) -> usize {
        self.0.get() as u32 as usize
    }
    fn generation(self) -> u32 {
        (self.0.get() >> 32) as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaintError {
    /// Empty, reversed, overlapping, or unsorted range at this index.
    InvalidRange { index: usize },
    /// The explicit-ownership pool has exhausted its 131,072 slots.
    PoolFull,
}
impl std::fmt::Display for PaintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRange { index } => write!(f, "invalid paint range at index {index}"),
            Self::PoolFull => write!(f, "paint pool is full"),
        }
    }
}
impl std::error::Error for PaintError {}

struct Slot {
    generation: u32,
    spans: Option<Box<[PaintSpan]>>,
}
#[derive(Default)]
pub(crate) struct PaintPool {
    slots: Vec<Slot>,
    free: Vec<usize>,
}
const MAX_PAINTS: usize = 1 << 17;

impl PaintPool {
    pub fn register(&mut self, spans: &[PaintSpan]) -> Result<PaintHandle, PaintError> {
        let mut end = 0;
        for (index, s) in spans.iter().enumerate() {
            if s.range.start >= s.range.end || s.range.start < end {
                return Err(PaintError::InvalidRange { index });
            }
            end = s.range.end;
        }
        let index = if let Some(index) = self.free.pop() {
            index
        } else {
            if self.slots.len() == MAX_PAINTS {
                return Err(PaintError::PoolFull);
            }
            self.slots.push(Slot {
                generation: 1,
                spans: None,
            });
            self.slots.len() - 1
        };
        let slot = &mut self.slots[index];
        slot.spans = Some(spans.into());
        crate::work::count!(paint_registrations, 1);
        crate::work::count!(paint_span_copies, spans.len());
        Ok(PaintHandle::new(index, slot.generation))
    }
    pub fn get(&self, handle: PaintHandle) -> Option<&[PaintSpan]> {
        let slot = self.slots.get(handle.slot())?;
        (slot.generation == handle.generation()).then_some(())?;
        slot.spans.as_deref()
    }
    pub fn drop(&mut self, handle: PaintHandle) {
        let Some(slot) = self.slots.get_mut(handle.slot()) else {
            return;
        };
        if slot.generation != handle.generation() || slot.spans.is_none() {
            return;
        }
        slot.spans = None;
        crate::work::count!(paint_releases, 1);
        // Retire rather than wrap: even billions of releases cannot alias a handle.
        if let Some(next) = slot.generation.checked_add(1) {
            slot.generation = next;
            self.free.push(handle.slot());
        }
    }
    pub fn clear(&mut self) {
        for i in 0..self.slots.len() {
            if self.slots[i].spans.is_some() {
                self.drop(PaintHandle::new(i, self.slots[i].generation));
            }
        }
    }
}

/// One forward grapheme walk; no scan at all for unstyled paragraphs.
pub(crate) fn valid_font_spans(text: &str, spans: &[FontSpan]) -> bool {
    if spans.is_empty() {
        return true;
    }
    let mut boundaries = text
        .grapheme_indices(true)
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .peekable();
    let mut end = 0;
    for span in spans {
        if span.range.start >= span.range.end
            || span.range.start < end
            || span.range.end > text.len()
        {
            return false;
        }
        for edge in [span.range.start, span.range.end] {
            while boundaries.peek().is_some_and(|&b| b < edge) {
                boundaries.next();
            }
            if boundaries.peek() != Some(&edge) {
                return false;
            }
        }
        end = span.range.end;
    }
    true
}

/// Cache both spans and gaps. Binary search on interval changes handles clipped
/// starts and nonmonotonic (e.g. RTL) clusters without an O(glyphs * spans) scan.
pub(crate) struct PaintCursor<'a> {
    spans: &'a [PaintSpan],
    interval: Range<usize>,
    color: Color,
    base: Color,
}
impl<'a> PaintCursor<'a> {
    pub fn new(spans: &'a [PaintSpan], base: Color) -> Self {
        Self {
            spans,
            interval: 0..0,
            color: base,
            base,
        }
    }
    pub fn at(&mut self, byte: usize) -> Color {
        crate::work::count!(paint_lookups, 1);
        if !self.interval.contains(&byte) {
            crate::work::count!(paint_searches, 1);
            let index = self.spans.partition_point(|s| s.range.end <= byte);
            if let Some(span) = self.spans.get(index).filter(|s| s.range.start <= byte) {
                self.interval = span.range.clone();
                self.color = span.color;
            } else {
                let start = index
                    .checked_sub(1)
                    .map(|i| self.spans[i].range.end)
                    .unwrap_or(0);
                let end = self
                    .spans
                    .get(index)
                    .map(|s| s.range.start)
                    .unwrap_or(usize::MAX);
                self.interval = start..end;
                self.color = self.base;
            }
        }
        self.color
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn span(range: Range<usize>) -> PaintSpan {
        PaintSpan {
            range,
            color: Color([1., 0., 0., 1.]),
        }
    }
    #[test]
    fn pool_owns_validated_snapshots_and_never_aliases_reuse_or_clear() {
        let mut pool = PaintPool::default();
        let mut input = vec![span(1..3)];
        let a = pool.register(&input).unwrap();
        input[0].range = 4..9;
        assert_eq!(pool.get(a).unwrap()[0].range, 1..3);
        assert!(pool.register(&[span(3..4), span(2..3)]).is_err());
        assert!(pool.register(&[span(3..3)]).is_err());
        pool.drop(a);
        let b = pool.register(&input).unwrap();
        assert_ne!(a, b);
        assert!(pool.get(a).is_none());
        pool.drop(a);
        assert!(pool.get(b).is_some());
        pool.clear();
        let c = pool.register(&[]).unwrap();
        assert!(pool.get(b).is_none());
        assert_ne!(b, c);
    }
    #[test]
    fn generation_exhaustion_retires_slot() {
        let mut pool = PaintPool::default();
        let a = pool.register(&[]).unwrap();
        pool.slots[a.slot()].generation = u32::MAX;
        pool.drop(PaintHandle::new(a.slot(), u32::MAX));
        assert_ne!(pool.register(&[]).unwrap().slot(), a.slot());
    }
    #[test]
    fn color_lookup_covers_gaps_clipping_and_reverse_clusters() {
        let base = Color([0., 0., 0., 1.]);
        let spans = [span(2..5), span(9..12)];
        let mut cursor = PaintCursor::new(&spans, base);
        for i in [10, 11, 30, 31, 1, 3, 5, 8, 9, 2, 0] {
            let expected = if (2..5).contains(&i) || (9..12).contains(&i) {
                spans[0].color
            } else {
                base
            };
            assert_eq!(cursor.at(i), expected);
        }
    }
    #[test]
    fn exhaustion_does_not_evict_owned_snapshots() {
        let mut pool = PaintPool::default();
        let first = pool.register(&[]).unwrap();
        for _ in 1..MAX_PAINTS {
            pool.register(&[]).unwrap();
        }
        assert_eq!(pool.register(&[]), Err(PaintError::PoolFull));
        assert!(pool.get(first).is_some());
        pool.drop(first);
        assert!(pool.register(&[]).is_ok());
        assert!(pool.get(first).is_none());
    }
}
