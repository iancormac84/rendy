use std::ops::Range;

use ash::{version::DeviceV1_0, vk};

use allocator::*;
use smallvec::SmallVec;

use block::Block;
use error::*;
use mapping::*;
use usage::{MemoryUsage, MemoryUsageValue};
use util::*;

/// Config for `Heaps` allocator.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct HeapsConfig {
    /// Config for arena sub-allocator.
    pub arena: Option<ArenaConfig>,

    /// Config for dynamic sub-allocator.
    pub dynamic: Option<DynamicConfig>,
}

/// Heaps available on particular physical device.
#[derive(Debug)]
pub struct Heaps {
    types: Vec<MemoryType>,
    heaps: Vec<MemoryHeap>,
}

impl Heaps {
    /// This must be called with `vk::MemoryPropertyFlags` fetched from physical device.
    pub unsafe fn new<P, H>(types: P, heaps: H) -> Self
    where
        P: IntoIterator<Item = (vk::MemoryPropertyFlags, u32, HeapsConfig)>,
        H: IntoIterator<Item = u64>,
    {
        let heaps = heaps
            .into_iter()
            .map(|size| MemoryHeap::new(size))
            .collect::<Vec<_>>();
        Heaps {
            types: types
                .into_iter()
                .enumerate()
                .map(|(index, (properties, heap_index, config))| {
                    assert!(
                        fits_u32(index),
                        "Number of memory types must fit in u32 limit"
                    );
                    assert!(
                        fits_usize(heap_index),
                        "Number of memory types must fit in u32 limit"
                    );
                    let memory_type = index as u32;
                    let heap_index = heap_index as usize;
                    assert!(heap_index < heaps.len());
                    MemoryType::new(memory_type, heap_index, properties, config)
                }).collect(),
            heaps,
        }
    }

    /// Allocate memory block
    /// from one of memory types specified by `mask`,
    /// for intended `usage`,
    /// with `size`
    /// and `align` requirements.
    pub fn allocate(
        &mut self,
        device: &impl DeviceV1_0,
        mask: u32,
        usage: impl MemoryUsage,
        size: u64,
        align: u64,
    ) -> Result<MemoryBlock, MemoryError> {
        debug_assert!(fits_u32(self.types.len()));

        let (memory_index, _, _) = {
            let suitable_types = self
                .types
                .iter()
                .enumerate()
                .filter(|(index, _)| (mask & (1u32 << index)) != 0)
                .filter_map(|(index, mt)| {
                    usage
                        .memory_fitness(mt.properties)
                        .map(move |fitness| (index, mt, fitness))
                }).collect::<SmallVec<[_; 64]>>();

            if suitable_types.is_empty() {
                return Err(AllocationError::NoSuitableMemory(mask, usage.value()).into());
            }

            suitable_types
                .into_iter()
                .filter(|(_, mt, _)| self.heaps[mt.heap_index].available() > size + align)
                .max_by_key(|&(_, _, fitness)| fitness)
                .ok_or(OutOfMemoryError::HeapsExhausted)?
        };

        self.allocate_from(device, memory_index as u32, usage, size, align)
    }

    /// Allocate memory block
    /// from `memory_index` specified,
    /// for intended `usage`,
    /// with `size`
    /// and `align` requirements.
    fn allocate_from(
        &mut self,
        device: &impl DeviceV1_0,
        memory_index: u32,
        usage: impl MemoryUsage,
        size: u64,
        align: u64,
    ) -> Result<MemoryBlock, MemoryError> {
        // trace!("Alloc block: type '{}', usage '{:#?}', size: '{}', align: '{}'", memory_index, usage.value(), size, align);
        assert!(fits_usize(memory_index));

        let ref mut memory_type = self.types[memory_index as usize];
        let ref mut memory_heap = self.heaps[memory_type.heap_index];

        if memory_heap.available() < size {
            return Err(OutOfMemoryError::HeapsExhausted.into());
        }

        let (block, allocated) = memory_type.alloc(device, usage, size, align)?;
        memory_heap.used += allocated;

        Ok(MemoryBlock {
            block,
            memory_index,
        })
    }

    /// Free memory block.
    ///
    /// Memory block must be allocated from this heap.
    pub fn free(&mut self, device: &impl DeviceV1_0, block: MemoryBlock) {
        // trace!("Free block '{:#?}'", block);
        let memory_index = block.memory_index;
        debug_assert!(fits_usize(memory_index));

        let ref mut memory_type = self.types[memory_index as usize];
        let ref mut memory_heap = self.heaps[memory_type.heap_index];
        let freed = memory_type.free(device, block.block);
        memory_heap.used -= freed;
    }

    /// Dispose of allocator.
    /// Cleanup allocators before dropping.
    /// Will panic if memory instances are left allocated.
    pub fn dispose(self, device: &impl DeviceV1_0) {
        for mt in self.types {
            mt.dispose(device)
        }
    }
}

/// Memory block allocated from `Heaps`.
#[derive(Debug)]
pub struct MemoryBlock {
    block: BlockFlavor,
    memory_index: u32,
}

impl MemoryBlock {
    /// Get memory type id.
    pub fn memory_type(&self) -> u32 {
        self.memory_index
    }
}

#[derive(Debug)]
enum BlockFlavor {
    Dedicated(DedicatedBlock),
    Arena(ArenaBlock),
    Dynamic(DynamicBlock),
    // Chunk(ChunkBlock),
}

macro_rules! any_block {
    ($self:ident. $block:ident => $expr:expr) => {{
        use self::BlockFlavor::*;
        match $self.$block {
            Dedicated($block) => $expr,
            Arena($block) => $expr,
            Dynamic($block) => $expr,
            // Chunk($block) => $expr,
        }
    }};
    (& $self:ident. $block:ident => $expr:expr) => {{
        use self::BlockFlavor::*;
        match &$self.$block {
            Dedicated($block) => $expr,
            Arena($block) => $expr,
            Dynamic($block) => $expr,
            // Chunk($block) => $expr,
        }
    }};
    (&mut $self:ident. $block:ident => $expr:expr) => {{
        use self::BlockFlavor::*;
        match &mut $self.$block {
            Dedicated($block) => $expr,
            Arena($block) => $expr,
            Dynamic($block) => $expr,
            // Chunk($block) => $expr,
        }
    }};
}

impl Block for MemoryBlock {
    #[inline]
    fn properties(&self) -> vk::MemoryPropertyFlags {
        any_block!(&self.block => block.properties())
    }

    #[inline]
    fn memory(&self) -> vk::DeviceMemory {
        any_block!(&self.block => block.memory())
    }

    #[inline]
    fn range(&self) -> Range<u64> {
        any_block!(&self.block => block.range())
    }

    fn map<'a>(
        &'a mut self,
        device: &impl DeviceV1_0,
        range: Range<u64>,
    ) -> Result<MappedRange<'a>, MappingError> {
        any_block!(&mut self.block => block.map(device, range))
    }

    fn unmap(&mut self, device: &impl DeviceV1_0) {
        any_block!(&mut self.block => block.unmap(device))
    }
}

#[derive(Debug)]
struct MemoryHeap {
    size: u64,
    used: u64,
}

impl MemoryHeap {
    fn new(size: u64) -> Self {
        MemoryHeap { size, used: 0 }
    }

    fn available(&self) -> u64 {
        self.size - self.used
    }
}

#[derive(Debug)]
struct MemoryType {
    heap_index: usize,
    properties: vk::MemoryPropertyFlags,
    dedicated: DedicatedAllocator,
    arena: Option<ArenaAllocator>,
    dynamic: Option<DynamicAllocator>,
    // chunk: Option<ChunkAllocator>,
}

impl MemoryType {
    fn new(
        memory_type: u32,
        heap_index: usize,
        properties: vk::MemoryPropertyFlags,
        config: HeapsConfig,
    ) -> Self {
        MemoryType {
            properties,
            heap_index,
            dedicated: DedicatedAllocator::new(memory_type, properties),
            arena: if properties.subset(ArenaAllocator::properties_required()) {
                config
                    .arena
                    .map(|config| ArenaAllocator::new(memory_type, properties, config))
            } else {
                None
            },
            dynamic: if properties.subset(DynamicAllocator::properties_required()) {
                config
                    .dynamic
                    .map(|config| DynamicAllocator::new(memory_type, properties, config))
            } else {
                None
            },
        }
    }

    fn alloc(
        &mut self,
        device: &impl DeviceV1_0,
        usage: impl MemoryUsage,
        size: u64,
        align: u64,
    ) -> Result<(BlockFlavor, u64), MemoryError> {
        match (usage.value(), self.arena.as_mut(), self.dynamic.as_mut()) {
            (MemoryUsageValue::Upload, Some(ref mut arena), _)
            | (MemoryUsageValue::Download, Some(ref mut arena), _)
                if size <= arena.max_allocation() =>
            {
                arena
                    .alloc(device, size, align)
                    .map(|(block, allocated)| (BlockFlavor::Arena(block), allocated))
            }
            (MemoryUsageValue::Dynamic, _, Some(ref mut dynamic))
                if size <= dynamic.max_allocation() =>
            {
                dynamic
                    .alloc(device, size, align)
                    .map(|(block, allocated)| (BlockFlavor::Dynamic(block), allocated))
            }
            (MemoryUsageValue::Data, _, Some(ref mut dynamic))
                if size <= dynamic.max_allocation() =>
            {
                dynamic
                    .alloc(device, size, align)
                    .map(|(block, allocated)| (BlockFlavor::Dynamic(block), allocated))
            }
            _ => self
                .dedicated
                .alloc(device, size, align)
                .map(|(block, allocated)| (BlockFlavor::Dedicated(block), allocated)),
        }
    }

    fn free(&mut self, device: &impl DeviceV1_0, block: BlockFlavor) -> u64 {
        match block {
            BlockFlavor::Dedicated(block) => self.dedicated.free(device, block),
            BlockFlavor::Arena(block) => self.arena.as_mut().unwrap().free(device, block),
            BlockFlavor::Dynamic(block) => self.dynamic.as_mut().unwrap().free(device, block),
        }
    }

    fn dispose(self, device: &impl DeviceV1_0) {
        if let Some(arena) = self.arena {
            arena.dispose(device);
        }
        if let Some(dynamic) = self.dynamic {
            dynamic.dispose();
        }
    }
}
