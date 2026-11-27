use super::*;
use crate::metal::{
    nvidia::{CHIPS, fixture::nvk},
    vulkan::{MemoryHeapFlags, physical::MemoryHeap, physical::MemoryType},
};
use alloc::collections::BTreeMap;

/// A GPU that runs a check's copy and fill on memory it keeps, whose
/// device-local memory may drop lines the CPU writes through its mapping,
/// as the AD107 laptop's GPU read zeros where NVK wrote its shaders.
struct Gpu {
    memory: physical::Memory,
    next: u64,
    /// Each memory's bytes and whether it is the GPU's own.
    memories: BTreeMap<u64, (Vec<u8>, bool)>,
    /// The memory under each buffer.
    buffers: BTreeMap<u64, u64>,
    /// Every `drop_every`th line the CPU writes to device-local memory
    /// never reaches it.
    drop_every: Option<usize>,
    /// Whether the GPU finishes what is submitted.
    finishes: bool,
    /// The failure a refused submission leaves on its signal.
    refuses: Option<Error>,
    submitted: Vec<Command>,
    destroyed: Vec<Object>,
}

impl Gpu {
    fn new() -> Self {
        Self {
            memory: nvk(&CHIPS[0], 8 << 30).memory,
            next: 0,
            memories: BTreeMap::new(),
            buffers: BTreeMap::new(),
            drop_every: None,
            finishes: true,
            refuses: None,
            submitted: Vec::new(),
            destroyed: Vec::new(),
        }
    }

    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn bytes(&mut self, buffer: Buffer) -> &mut Vec<u8> {
        let memory = self.buffers[&buffer.0];
        &mut self.memories.get_mut(&memory).unwrap().0
    }
}

impl Probe for Gpu {
    fn memory(&self) -> &physical::Memory {
        &self.memory
    }
    fn allocate_memory(
        &mut self,
        bytes: u64,
        request: MemoryRequest,
        types: u32,
    ) -> Result<Memory> {
        assert!(request.mapped);
        let index = (0..self.memory.types.len())
            .find(|&index| {
                types & 1 << index != 0
                    && self.memory.types[index]
                        .properties
                        .contains(request.required)
            })
            .expect("a type of the mask holds the required properties");
        let local = self.memory.types[index]
            .properties
            .contains(MemoryProperties::DEVICE_LOCAL);
        let id = self.id();
        self.memories.insert(id, (vec![0; bytes as usize], local));
        Ok(Memory(id))
    }
    fn create_placed_buffer(&mut self, info: &BufferInfo, memory: Memory) -> Result<Buffer> {
        assert_eq!(info.size as usize, self.memories[&memory.0].0.len());
        let id = self.id();
        self.buffers.insert(id, memory.0);
        Ok(Buffer(id))
    }
    fn write_memory(&mut self, memory: Memory, offset: u64, bytes: &[u8]) -> Result<()> {
        let drop_every = self.drop_every;
        let (stored, local) = self.memories.get_mut(&memory.0).unwrap();
        for (index, line) in bytes.chunks(LINE).enumerate() {
            if *local && drop_every.is_some_and(|every| index % every == 0) {
                continue;
            }
            let at = offset as usize + index * LINE;
            stored[at..at + line.len()].copy_from_slice(line);
        }
        Ok(())
    }
    fn read_memory(&mut self, memory: Memory, offset: u64, bytes: &mut [u8]) -> Result<()> {
        let stored = &self.memories[&memory.0].0;
        bytes.copy_from_slice(&stored[offset as usize..offset as usize + bytes.len()]);
        Ok(())
    }
    fn create_semaphore(&mut self) -> Result<Semaphore> {
        Ok(Semaphore(self.id()))
    }
    fn submit(&mut self, segment: &Segment) -> Result<()> {
        assert_eq!(segment.signals.len(), 1);
        if !self.finishes || self.refuses.is_some() {
            return Ok(());
        }
        for command in &segment.commands {
            match *command {
                Command::CopyBuffer { src, dst, region } => {
                    let from = region.src_offset as usize;
                    let copied = self.bytes(src)[from..from + region.size as usize].to_vec();
                    let to = region.dst_offset as usize;
                    self.bytes(dst)[to..to + copied.len()].copy_from_slice(&copied);
                }
                Command::FillBuffer {
                    buffer,
                    offset,
                    size,
                    data,
                } => {
                    let range = offset as usize..(offset + size) as usize;
                    for word in self.bytes(buffer)[range].chunks_mut(4) {
                        word.copy_from_slice(&data.to_le_bytes());
                    }
                }
                Command::Barrier(_) => {}
                ref other => panic!("a check submits no {other:?}"),
            }
        }
        self.submitted.extend(segment.commands.iter().cloned());
        Ok(())
    }
    fn wait_semaphore(&mut self, _: Semaphore, _: u64) -> Result<bool> {
        Ok(self.finishes)
    }
    fn submit_failure(&mut self, _: Semaphore, _: u64) -> Option<Error> {
        self.refuses.take()
    }
    fn destroy(&mut self, object: Object) {
        self.destroyed.push(object);
    }
}

#[test]
fn a_gpu_that_sees_every_line_passes_both_ways() {
    let mut gpu = Gpu::new();
    let checked = check(&mut gpu);
    let all = Seen {
        lines: 1024,
        ..Seen::default()
    };
    assert_eq!(checked, Check::Compared { gpu: all, cpu: all });
    assert!(!checked.unseen());
    assert_eq!(
        checked.line(),
        "mapped device memory: the GPU saw 1024 of 1024 64-byte lines the CPU wrote; \
         the CPU saw 1024 of 1024 64-byte lines the GPU wrote"
    );
    // The two memories, their buffers and the semaphore are released.
    assert_eq!(gpu.destroyed.len(), 5);
    assert_eq!(gpu.submitted.len(), 3);
}

#[test]
fn lines_the_cpu_wrote_that_never_reached_device_memory_are_counted_and_shown() {
    let mut gpu = Gpu::new();
    gpu.drop_every = Some(4);
    let checked = check(&mut gpu);
    assert!(checked.unseen());
    let Check::Compared { gpu: seen, cpu } = checked else {
        panic!("{checked:?}");
    };
    assert_eq!(seen.lines, 1024);
    assert_eq!(seen.differing, 256);
    // Memory the CPU's writes missed holds what it held: zero.
    assert_eq!(seen.zero, 256);
    assert_eq!(seen.first, Some((0, word(0), 0)));
    // The GPU's own fill reaches the CPU.
    assert_eq!(cpu.differing, 0);
    assert_eq!(
        checked.line(),
        alloc::format!(
            "mapped device memory: the GPU saw 768 of 1024 64-byte lines the CPU wrote \
             (first differing word at +0x0: {:#010x} written, 0x00000000 read; 256 of the 256 \
             differing lines read zero); the CPU saw 1024 of 1024 64-byte lines the GPU wrote",
            word(0)
        )
    );
}

#[test]
fn a_device_without_mapped_device_memory_or_mapped_system_memory_is_not_checked() {
    let mut gpu = Gpu::new();
    gpu.memory.types.retain(|kind| {
        !kind
            .properties
            .contains(MemoryProperties::DEVICE_LOCAL | MemoryProperties::HOST_VISIBLE)
    });
    let checked = check(&mut gpu);
    assert_eq!(
        checked,
        Check::Untested("the CPU maps none of the GPU's own memory")
    );
    assert!(!checked.unseen());
    assert_eq!(
        checked.line(),
        "mapped device memory not checked: the CPU maps none of the GPU's own memory"
    );

    // An integrated GPU: all of its memory is its own.
    let mut gpu = Gpu::new();
    gpu.memory = physical::Memory {
        heaps: vec![MemoryHeap {
            size: 4 << 30,
            flags: MemoryHeapFlags::DEVICE_LOCAL,
            budget: None,
        }],
        types: vec![MemoryType {
            properties: MemoryProperties::DEVICE_LOCAL
                | MemoryProperties::HOST_VISIBLE
                | MemoryProperties::HOST_COHERENT,
            heap_index: 0,
        }],
    };
    assert_eq!(
        check(&mut gpu),
        Check::Untested("the CPU maps no system memory of the GPU's")
    );
    assert!(gpu.memories.is_empty());
}

#[test]
fn a_copy_that_never_finishes_keeps_its_memory_for_the_gpu() {
    let mut gpu = Gpu::new();
    gpu.finishes = false;
    let checked = check(&mut gpu);
    assert_eq!(checked, Check::Unfinished);
    assert!(!checked.unseen());
    assert!(gpu.destroyed.is_empty());
    assert_eq!(
        checked.line(),
        "mapped device memory not checked: the GPU's copy did not finish within 2 s"
    );
}

#[test]
fn a_refused_copy_is_a_failure_that_releases_what_it_made() {
    let mut gpu = Gpu::new();
    gpu.refuses = Some(Error::DeviceLost);
    let checked = check(&mut gpu);
    assert_eq!(checked, Check::Failed(Error::DeviceLost));
    assert!(!checked.unseen());
    assert_eq!(gpu.destroyed.len(), 5);
    assert_eq!(
        checked.line(),
        "mapped device memory not checked: DeviceLost"
    );
}

#[test]
fn the_check_copies_from_mapped_device_memory_into_mapped_system_memory() {
    let memory = nvk(&CHIPS[0], 8 << 30).memory;
    let mapped = MemoryProperties::DEVICE_LOCAL | MemoryProperties::HOST_VISIBLE;
    // The fixture's fourth type is device-local and mapped; its second and
    // third are system memory.
    assert_eq!(types(&memory, mapped, MemoryProperties::EMPTY), 0b1000);
    assert_eq!(
        types(
            &memory,
            MemoryProperties::HOST_VISIBLE,
            MemoryProperties::DEVICE_LOCAL
        ),
        0b0110
    );
    let mut gpu = Gpu::new();
    check(&mut gpu);
    let local: Vec<bool> = gpu.memories.values().map(|&(_, local)| local).collect();
    assert_eq!(local, [true, false]);
    let commands = commands(Buffer(1), Buffer(2));
    assert!(matches!(
        commands.last(),
        Some(Command::Barrier(Barrier {
            dst_stages: PipelineStages::HOST,
            dst_access: Access::HOST_READ,
            ..
        }))
    ));
}

#[test]
fn every_written_word_is_nonzero_unlike_the_fill_and_each_line_differs() {
    let words: Vec<u32> = (0..(BYTES / 4) as usize).map(word).collect();
    assert!(words.iter().all(|&word| word != 0 && word != FILL));
    let bytes = pattern(BYTES as usize);
    assert_eq!(bytes.len() as u64, BYTES);
    let mut lines: Vec<&[u8]> = bytes.chunks(LINE).collect();
    lines.sort_unstable();
    lines.dedup();
    assert_eq!(lines.len(), 1024);
}

#[test]
fn a_comparison_names_the_first_differing_word_within_its_line() {
    let written = pattern(4 * LINE);
    let mut read = written.clone();
    // A stale word in the third line, and a fourth line that read zero.
    read[2 * LINE + 8..2 * LINE + 12].copy_from_slice(&7u32.to_le_bytes());
    read[3 * LINE..].fill(0);
    let seen = Seen::compare(&written, &read);
    assert_eq!(
        seen,
        Seen {
            lines: 4,
            differing: 2,
            zero: 1,
            first: Some((2 * LINE + 8, word(2 * LINE / 4 + 2), 7)),
        }
    );
}
