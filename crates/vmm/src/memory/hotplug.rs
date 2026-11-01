use std::collections::BTreeSet;
use std::ops;

use crate::align::{Alignment, BackingPageSize};
use crate::memory::layout::{MemoryLayout, Window, WindowId, WindowKind, WindowOwner};
use crate::space::{Gpa, ObjectOffset, Range};

pub const DEFAULT_BLOCK_SIZE: u64 = 2 << 20;
pub const DEFAULT_CHUNK_SIZE: u64 = 128 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum GeometryError {
    /// window {0} is not part of the memory layout
    UnknownWindow(WindowId),
    /// window {0} is not virtio-mem RAM
    NotVirtioMem(WindowId),
    /// block size {0:#x} is not a power of two
    BlockSize(u64),
    /// chunk size {0:#x} is not a power of two
    ChunkSize(u64),
    /// block size {block:#x} is smaller than backing page {page:#x}
    BelowBackingPage { block: u64, page: u64 },
    /// chunk size {chunk:#x} is not a multiple of block size {block:#x}
    ChunkMultiple { block: u64, chunk: u64 },
    /// window {0} does not contain a nonzero whole number of aligned chunks
    Region(WindowId),
    /// object slice of window {0} is not aligned to the backing page
    BackingUnaligned(WindowId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemGeometry {
    id: WindowId,
    window: Window,
    block: Alignment,
    chunk: Alignment,
}

impl MemGeometry {
    pub fn new(
        layout: &MemoryLayout,
        id: WindowId,
        block_size: u64,
        chunk_size: u64,
        backing_page: BackingPageSize,
    ) -> Result<Self, GeometryError> {
        let window = *layout.window(id).ok_or(GeometryError::UnknownWindow(id))?;
        if window.owner() != WindowOwner::VirtioMem || window.kind() != WindowKind::VirtioMemRam {
            return Err(GeometryError::NotVirtioMem(id));
        }
        let block = Alignment::new(block_size).ok_or(GeometryError::BlockSize(block_size))?;
        let chunk = Alignment::new(chunk_size).ok_or(GeometryError::ChunkSize(chunk_size))?;
        let page = backing_page.alignment();
        if block_size < page.get() {
            return Err(GeometryError::BelowBackingPage {
                block: block_size,
                page: page.get(),
            });
        }
        if chunk_size % block_size != 0 {
            return Err(GeometryError::ChunkMultiple {
                block: block_size,
                chunk: chunk_size,
            });
        }
        if window.placement().is_empty() || !window.placement().is_aligned(chunk) {
            return Err(GeometryError::Region(id));
        }
        if !window.slice().is_aligned(page) {
            return Err(GeometryError::BackingUnaligned(id));
        }
        Ok(Self {
            id,
            window,
            block,
            chunk,
        })
    }

    pub fn window_id(self) -> WindowId {
        self.id
    }

    pub fn window(self) -> Window {
        self.window
    }

    pub fn region(self) -> Range<Gpa> {
        self.window.placement()
    }

    pub fn slice(self) -> Range<ObjectOffset> {
        self.window.slice()
    }

    pub fn block_size(self) -> u64 {
        self.block.get()
    }

    pub fn chunk_size(self) -> u64 {
        self.chunk.get()
    }

    pub fn block_count(self) -> u64 {
        self.region().len() / self.block_size()
    }

    pub fn chunk_count(self) -> u64 {
        self.region().len() / self.chunk_size()
    }

    pub fn chunk(self, ordinal: u64) -> Option<MemorySlice> {
        (ordinal < self.chunk_count())
            .then(|| self.span(ordinal * self.blocks_per_chunk(), self.blocks_per_chunk()))
    }

    fn blocks_per_chunk(self) -> u64 {
        self.chunk_size() / self.block_size()
    }

    fn blocks(self, range: Range<Gpa>) -> Result<ops::Range<u64>, PopulationError> {
        if range.is_empty() {
            return Err(PopulationError::EmptyRange);
        }
        if !self.region().contains_range(range) {
            return Err(PopulationError::OutsideGeometry(range));
        }
        if !range.is_aligned(self.block) {
            return Err(PopulationError::UnalignedRange {
                range,
                block: self.block_size(),
            });
        }
        let start = (range.start().0 - self.region().start().0) / self.block_size();
        Ok(start..start + range.len() / self.block_size())
    }

    fn span(self, start: u64, count: u64) -> MemorySlice {
        let offset = start * self.block_size();
        let len = count * self.block_size();
        MemorySlice {
            window: self.id,
            slice: Range::new(ObjectOffset(self.slice().start().0 + offset), len)
                .expect("validated blocks lie inside the object slice"),
            placement: Range::new(Gpa(self.region().start().0 + offset), len)
                .expect("validated blocks lie inside the guest region"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySlice {
    window: WindowId,
    slice: Range<ObjectOffset>,
    placement: Range<Gpa>,
}

impl MemorySlice {
    pub fn window(self) -> WindowId {
        self.window
    }

    pub fn slice(self) -> Range<ObjectOffset> {
        self.slice
    }

    pub fn placement(self) -> Range<Gpa> {
        self.placement
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    Plugged,
    Unplugged,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, displaydoc::Display, thiserror::Error)]
pub enum PopulationError {
    /// block range is empty
    EmptyRange,
    /// block range {0} is outside the virtio-mem geometry
    OutsideGeometry(Range<Gpa>),
    /// block range {range} is not aligned to block size {block:#x}
    UnalignedRange { range: Range<Gpa>, block: u64 },
    /// block range {0} contains already plugged blocks
    AlreadyPlugged(Range<Gpa>),
    /// block range {0} contains unplugged blocks
    NotPlugged(Range<Gpa>),
    /// restored population lists block {0} twice
    DuplicateBlock(Gpa),
    /// target limit {target:#x} is not a block-aligned size within region size {region:#x}
    InvalidTarget { target: u64, region: u64 },
    /// plugged size overflows
    SizeOverflow,
    /// plugged size {plugged:#x} would exceed target limit {target:#x}
    TargetExceeded { plugged: u64, target: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Population {
    geometry: MemGeometry,
    plugged: BTreeSet<u64>,
}

impl Population {
    pub fn new(geometry: MemGeometry) -> Self {
        Self {
            geometry,
            plugged: BTreeSet::new(),
        }
    }

    pub fn restore(
        geometry: MemGeometry,
        ranges: impl IntoIterator<Item = Range<Gpa>>,
    ) -> Result<Self, PopulationError> {
        let mut plugged = BTreeSet::new();
        for range in ranges {
            for block in geometry.blocks(range)? {
                if !plugged.insert(block) {
                    return Err(PopulationError::DuplicateBlock(
                        geometry.span(block, 1).placement().start(),
                    ));
                }
            }
        }
        Ok(Self { geometry, plugged })
    }

    pub fn geometry(&self) -> MemGeometry {
        self.geometry
    }

    pub fn plugged_blocks(&self) -> u64 {
        self.plugged.len() as u64
    }

    pub fn plugged_size(&self) -> u64 {
        self.plugged_blocks() * self.geometry.block_size()
    }

    pub fn plugged_ranges(&self) -> impl Iterator<Item = Range<Gpa>> + '_ {
        runs(self.plugged.iter().copied()).map(|blocks| {
            self.geometry
                .span(blocks.start, blocks.end - blocks.start)
                .placement()
        })
    }

    pub fn populated_chunks(&self) -> impl Iterator<Item = MemorySlice> + '_ {
        self.chunk_ordinals().into_iter().map(|ordinal| {
            self.geometry
                .chunk(ordinal)
                .expect("plugged blocks lie inside a chunk")
        })
    }

    pub fn state(&self, range: Range<Gpa>) -> Result<BlockState, PopulationError> {
        let blocks = self.geometry.blocks(range)?;
        let count = self.plugged.range(blocks.clone()).count() as u64;
        Ok(if count == 0 {
            BlockState::Unplugged
        } else if count == blocks.end - blocks.start {
            BlockState::Plugged
        } else {
            BlockState::Mixed
        })
    }

    pub fn plug(
        &self,
        range: Range<Gpa>,
        target_limit: u64,
    ) -> Result<PopulationProposal, PopulationError> {
        let blocks = self.geometry.blocks(range)?;
        if target_limit > self.geometry.region().len()
            || target_limit % self.geometry.block_size() != 0
        {
            return Err(PopulationError::InvalidTarget {
                target: target_limit,
                region: self.geometry.region().len(),
            });
        }
        if self.plugged.range(blocks.clone()).next().is_some() {
            return Err(PopulationError::AlreadyPlugged(range));
        }
        let plugged_size = self
            .plugged_size()
            .checked_add(range.len())
            .ok_or(PopulationError::SizeOverflow)?;
        if plugged_size > target_limit {
            return Err(PopulationError::TargetExceeded {
                plugged: plugged_size,
                target: target_limit,
            });
        }
        let mut plugged = self.plugged.clone();
        plugged.extend(blocks);
        Ok(self.propose(plugged))
    }

    pub fn unplug(&self, range: Range<Gpa>) -> Result<PopulationProposal, PopulationError> {
        let blocks = self.geometry.blocks(range)?;
        if self.plugged.range(blocks.clone()).count() as u64 != blocks.end - blocks.start {
            return Err(PopulationError::NotPlugged(range));
        }
        let mut plugged = self.plugged.clone();
        for block in blocks {
            plugged.remove(&block);
        }
        Ok(self.propose(plugged))
    }

    pub fn unplug_all(&self) -> PopulationProposal {
        self.propose(BTreeSet::new())
    }

    fn chunk_ordinals(&self) -> BTreeSet<u64> {
        self.plugged
            .iter()
            .map(|block| block / self.geometry.blocks_per_chunk())
            .collect()
    }

    fn propose(&self, plugged: BTreeSet<u64>) -> PopulationProposal {
        let population = Self {
            geometry: self.geometry,
            plugged,
        };
        let old_chunks = self.chunk_ordinals();
        let new_chunks = population.chunk_ordinals();
        let chunk = |ordinal| {
            self.geometry
                .chunk(ordinal)
                .expect("populated chunk lies inside the geometry")
        };
        let mut effects = Vec::new();
        effects.extend(
            new_chunks
                .difference(&old_chunks)
                .map(|&ordinal| MemoryEffect::BindChunk(chunk(ordinal))),
        );
        effects.extend(
            old_chunks
                .difference(&new_chunks)
                .map(|&ordinal| MemoryEffect::UnbindChunk(chunk(ordinal))),
        );
        effects.extend(
            runs(self.plugged.difference(&population.plugged).copied()).map(|blocks| {
                MemoryEffect::Discard(self.geometry.span(blocks.start, blocks.end - blocks.start))
            }),
        );
        if self.plugged != population.plugged {
            effects.push(MemoryEffect::CommitPopulation);
        }
        PopulationProposal {
            population,
            effects,
        }
    }
}

fn runs(blocks: impl Iterator<Item = u64>) -> impl Iterator<Item = ops::Range<u64>> {
    let mut blocks = blocks.peekable();
    std::iter::from_fn(move || {
        let start = blocks.next()?;
        let mut end = start + 1;
        while blocks.peek() == Some(&end) {
            blocks.next();
            end += 1;
        }
        Some(start..end)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryEffect {
    BindChunk(MemorySlice),
    UnbindChunk(MemorySlice),
    Discard(MemorySlice),
    CommitPopulation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopulationProposal {
    population: Population,
    effects: Vec<MemoryEffect>,
}

impl PopulationProposal {
    pub fn population(&self) -> &Population {
        &self.population
    }

    pub fn effects(&self) -> &[MemoryEffect] {
        &self.effects
    }

    pub fn into_parts(self) -> (Population, Vec<MemoryEffect>) {
        (self.population, self.effects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::HostPageSize;
    use crate::memory::layout::{Object, ObjectId};

    const HOT: WindowId = WindowId(7);
    const START: Gpa = Gpa(1 << 40);
    const OFFSET: ObjectOffset = ObjectOffset(0x1000);
    const BLOCK: u64 = DEFAULT_BLOCK_SIZE;
    const CHUNK: u64 = 4 * BLOCK;
    const SIZE: u64 = 3 * CHUNK;

    fn backing(bytes: u64) -> BackingPageSize {
        BackingPageSize::base(HostPageSize::new(bytes).unwrap())
    }

    fn layout(start: Gpa, size: u64) -> MemoryLayout {
        let boot = MemoryLayout::boot(0x1000).unwrap();
        let hot = Window::new(
            ObjectId(1),
            Range::new(OFFSET, size).unwrap(),
            start,
            WindowOwner::VirtioMem,
            WindowKind::VirtioMemRam,
        )
        .unwrap();
        MemoryLayout::new(
            boot.objects().map(|(id, object)| (id, *object)).chain([(
                ObjectId(1),
                Object {
                    capacity: OFFSET.0 + size,
                },
            )]),
            boot.windows()
                .map(|(id, window)| (id, *window))
                .chain([(HOT, hot)]),
        )
        .unwrap()
    }

    fn geometry() -> MemGeometry {
        MemGeometry::new(&layout(START, SIZE), HOT, BLOCK, CHUNK, backing(0x1000)).unwrap()
    }

    fn range(start: u64, count: u64) -> Range<Gpa> {
        Range::new(Gpa(START.0 + start * BLOCK), count * BLOCK).unwrap()
    }

    #[test]
    fn geometry_keeps_the_fixed_window_and_object_coordinates() {
        let geometry = geometry();
        fn copy<T: Copy>(_: T) {}
        copy(geometry);
        assert_eq!(geometry.window_id(), HOT);
        assert_eq!(geometry.region(), Range::new(START, SIZE).unwrap());
        assert_eq!(geometry.slice(), Range::new(OFFSET, SIZE).unwrap());
        assert_eq!((geometry.block_count(), geometry.chunk_count()), (12, 3));
        let chunk = geometry.chunk(1).unwrap();
        assert_eq!(
            chunk.slice(),
            Range::new(ObjectOffset(OFFSET.0 + CHUNK), CHUNK).unwrap()
        );
        assert_eq!(chunk.placement(), range(4, 4));
        assert!(geometry.chunk(3).is_none());
        assert_eq!(DEFAULT_CHUNK_SIZE, 128 << 20);
    }

    #[test]
    fn invalid_geometry_is_rejected_before_population() {
        let layout = layout(START, SIZE);
        assert_eq!(
            MemGeometry::new(&layout, WindowId(99), BLOCK, CHUNK, backing(0x1000)),
            Err(GeometryError::UnknownWindow(WindowId(99)))
        );
        assert_eq!(
            MemGeometry::new(&layout, WindowId(0), BLOCK, CHUNK, backing(0x1000)),
            Err(GeometryError::NotVirtioMem(WindowId(0)))
        );
        for block in [0, 3, BLOCK + 1] {
            assert_eq!(
                MemGeometry::new(&layout, HOT, block, CHUNK, backing(0x1000)),
                Err(GeometryError::BlockSize(block))
            );
        }
        for chunk in [0, 3, CHUNK + 1] {
            assert_eq!(
                MemGeometry::new(&layout, HOT, BLOCK, chunk, backing(0x1000)),
                Err(GeometryError::ChunkSize(chunk))
            );
        }
        assert!(matches!(
            MemGeometry::new(&layout, HOT, 0x800, CHUNK, backing(0x1000)),
            Err(GeometryError::BelowBackingPage { .. })
        ));
        assert!(matches!(
            MemGeometry::new(&layout, HOT, CHUNK, BLOCK, backing(0x1000)),
            Err(GeometryError::ChunkMultiple { .. })
        ));
        assert_eq!(
            MemGeometry::new(
                &self::layout(Gpa(START.0 + BLOCK), SIZE),
                HOT,
                BLOCK,
                CHUNK,
                backing(0x1000)
            ),
            Err(GeometryError::Region(HOT))
        );
        assert_eq!(
            MemGeometry::new(
                &self::layout(START, SIZE - BLOCK),
                HOT,
                BLOCK,
                CHUNK,
                backing(0x1000)
            ),
            Err(GeometryError::Region(HOT))
        );
        assert_eq!(
            MemGeometry::new(&layout, HOT, BLOCK, CHUNK, backing(0x2000)),
            Err(GeometryError::BackingUnaligned(HOT))
        );
    }

    #[test]
    fn crossing_chunks_binds_once_and_only_empty_chunks_unbind() {
        let geometry = geometry();
        let empty = Population::new(geometry);
        let proposal = empty.plug(range(3, 2), SIZE).unwrap();
        assert_eq!(
            proposal.effects(),
            [
                MemoryEffect::BindChunk(geometry.chunk(0).unwrap()),
                MemoryEffect::BindChunk(geometry.chunk(1).unwrap()),
                MemoryEffect::CommitPopulation,
            ]
        );
        assert_eq!(empty.plugged_size(), 0);
        let populated = proposal.population();
        assert_eq!(populated.plugged_blocks(), 2);
        assert_eq!(populated.plugged_size(), 2 * BLOCK);
        let more = populated.plug(range(5, 1), SIZE).unwrap();
        assert_eq!(more.effects(), [MemoryEffect::CommitPopulation]);
        let partial = populated.unplug(range(3, 1)).unwrap();
        assert_eq!(
            partial.effects(),
            [
                MemoryEffect::UnbindChunk(geometry.chunk(0).unwrap()),
                MemoryEffect::Discard(geometry.span(3, 1)),
                MemoryEffect::CommitPopulation,
            ]
        );
        let within = more.population().unplug(range(4, 1)).unwrap();
        assert_eq!(
            within.effects(),
            [
                MemoryEffect::Discard(geometry.span(4, 1)),
                MemoryEffect::CommitPopulation
            ]
        );
        assert_eq!(
            within.population().populated_chunks().collect::<Vec<_>>(),
            [geometry.chunk(0).unwrap(), geometry.chunk(1).unwrap()]
        );
    }

    #[test]
    fn mixed_and_duplicate_changes_leave_the_original_population_intact() {
        let population = Population::restore(geometry(), [range(1, 2)]).unwrap();
        let before = population.clone();
        assert_eq!(population.state(range(1, 2)), Ok(BlockState::Plugged));
        assert_eq!(population.state(range(4, 1)), Ok(BlockState::Unplugged));
        assert_eq!(population.state(range(0, 4)), Ok(BlockState::Mixed));
        for range in [range(1, 2), range(0, 2)] {
            assert_eq!(
                population.plug(range, SIZE),
                Err(PopulationError::AlreadyPlugged(range))
            );
        }
        assert_eq!(
            population.unplug(range(0, 2)),
            Err(PopulationError::NotPlugged(range(0, 2)))
        );
        let proposal = population.unplug(range(1, 2)).unwrap();
        assert_eq!(
            proposal.population().unplug(range(1, 2)),
            Err(PopulationError::NotPlugged(range(1, 2)))
        );
        assert_eq!(population, before);
        assert_eq!(
            Population::restore(geometry(), [range(1, 2), range(2, 1)]),
            Err(PopulationError::DuplicateBlock(range(2, 1).start()))
        );
    }

    #[test]
    fn bounds_alignment_and_target_caps_are_checked_before_proposing() {
        let population = Population::new(geometry());
        for range in [
            Range::new(Gpa(START.0 - BLOCK), BLOCK).unwrap(),
            range(11, 2),
        ] {
            assert_eq!(
                population.plug(range, SIZE),
                Err(PopulationError::OutsideGeometry(range))
            );
            assert_eq!(
                population.unplug(range),
                Err(PopulationError::OutsideGeometry(range))
            );
            assert_eq!(
                Population::restore(geometry(), [range]),
                Err(PopulationError::OutsideGeometry(range))
            );
        }
        assert_eq!(
            population.state(range(0, 0)),
            Err(PopulationError::EmptyRange)
        );
        for range in [
            Range::new(Gpa(START.0 + 0x1000), BLOCK).unwrap(),
            Range::new(START, BLOCK + 0x1000).unwrap(),
        ] {
            assert!(matches!(
                population.plug(range, SIZE),
                Err(PopulationError::UnalignedRange { .. })
            ));
        }
        assert_eq!(
            population.plug(range(0, 2), BLOCK),
            Err(PopulationError::TargetExceeded {
                plugged: 2 * BLOCK,
                target: BLOCK
            })
        );
        for target in [SIZE + BLOCK, BLOCK + 1, u64::MAX] {
            assert_eq!(
                population.plug(range(0, 1), target),
                Err(PopulationError::InvalidTarget {
                    target,
                    region: SIZE
                })
            );
        }
        let populated = Population::restore(geometry(), [range(0, 2)]).unwrap();
        assert_eq!(
            populated.plug(range(2, 1), 0),
            Err(PopulationError::TargetExceeded {
                plugged: 3 * BLOCK,
                target: 0
            })
        );
        assert_eq!(
            populated
                .unplug(range(0, 1))
                .unwrap()
                .population()
                .plugged_size(),
            BLOCK
        );
        assert_eq!(population.plugged_size(), 0);
    }

    #[test]
    fn all_discards_only_plugged_runs_and_empty_all_has_no_memory_effect() {
        let geometry = geometry();
        let population =
            Population::restore(geometry, [range(0, 1), range(2, 1), range(7, 2)]).unwrap();
        assert_eq!(
            population.plugged_ranges().collect::<Vec<_>>(),
            [range(0, 1), range(2, 1), range(7, 2)]
        );
        assert_eq!(
            Population::restore(geometry, population.plugged_ranges()).unwrap(),
            population
        );
        let proposal = population.unplug_all();
        assert_eq!(
            proposal.effects(),
            [
                MemoryEffect::UnbindChunk(geometry.chunk(0).unwrap()),
                MemoryEffect::UnbindChunk(geometry.chunk(1).unwrap()),
                MemoryEffect::UnbindChunk(geometry.chunk(2).unwrap()),
                MemoryEffect::Discard(geometry.span(0, 1)),
                MemoryEffect::Discard(geometry.span(2, 1)),
                MemoryEffect::Discard(geometry.span(7, 2)),
                MemoryEffect::CommitPopulation,
            ]
        );
        assert_eq!(proposal.population().plugged_size(), 0);
        let empty = proposal.population().unplug_all();
        assert!(empty.effects().is_empty());
        assert_eq!(empty.population(), proposal.population());
    }
}
