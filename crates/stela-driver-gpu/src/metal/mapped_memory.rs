//! Whether the GPU sees what the CPU writes through a mapping of the GPU's
//! own memory, and the CPU what the GPU writes there: the path NVK takes
//! for every shader it uploads on a GPU whose BAR maps all of its memory
//! (`nvk_heap_upload`, "upload=mapped" in a fault dump) and for the compute
//! launch descriptors and constants a command buffer uploads.
//!
//! An RTX 4060 Laptop GPU (AD107) on a Meteor Lake MSI laptop lost its
//! device at the first draw of every WindowServer start with "Shader
//! Program Header 11/18 Error" (Xid 13), with "Illegal Instruction
//! Encoding" on every SM of a later one, and its compute dispatches with
//! SKEDCHECK06_REGISTER_COUNT and SKEDCHECK16_CTA_THREAD_DIMENSION_ZERO,
//! which a launch descriptor of zeros gives; the program headers and code
//! NAK wrote were valid and the same GPU with the same 8 GiB BAR drew on
//! another laptop. Its command buffers, which NVK keeps in system memory,
//! ran. Everything it read wrong is what the CPU wrote through the BAR, so
//! the likeliest cause is that those writes did not reach its memory as
//! written; this check tells that apart from a fault of the shader path.
//!
//! [`check`] writes a pattern through such a mapping, has the GPU's copy
//! engine copy it to system memory and fill a second range, and compares
//! both, before the device serves anything. [`Check::line`] says what it
//! saw for the boot log; [`Check::unseen`] is what moves a driver that can
//! (`native::Loader::system_memory`) to keep every
//! allocation in system memory, which the GPU reaches as it reaches its
//! command buffers.
use super::{
    command_buffer::Segment,
    error::{Error, Result},
    vulkan::{
        Access, BufferCreate, BufferUsage, MemoryProperties, PipelineStages,
        backend::{Backend, Object},
        command::{Barrier, BufferCopy, Command},
        object::{Buffer, BufferInfo, Memory, MemoryRequest, Semaphore},
        physical,
    },
};
use alloc::{format, string::String, vec, vec::Vec};

/// The bytes written each way: 1024 of the 64-byte lines a CPU's
/// write-combining buffer or cache writes back at once.
pub const BYTES: u64 = 64 << 10;
/// The bytes a line holds.
pub const LINE: usize = 64;
/// The word the GPU fills its range with.
pub const FILL: u32 = 0xa5c3_0f96;
/// How long the copy may take: a GPU whose rings run copies 128 KiB in
/// well under a millisecond.
pub const WAIT_NS: u64 = 2_000_000_000;

/// The word the CPU writes at word `index`: never zero, never [`FILL`], and
/// different in every line, so a line read from elsewhere shows.
pub const fn word(index: usize) -> u32 {
    let mixed = (index as u32).wrapping_mul(0x9e37_79b9) ^ 0x5a5a_5a5a;
    if mixed == 0 || mixed == FILL {
        0x0bad_f00d
    } else {
        mixed
    }
}

/// The CPU's pattern of `bytes` bytes.
pub fn pattern(bytes: usize) -> Vec<u8> {
    (0..bytes / 4)
        .flat_map(|index| word(index).to_le_bytes())
        .collect()
}

/// How what one side read differs from what the other wrote.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Seen {
    /// Lines compared.
    pub lines: u32,
    /// Lines that differed.
    pub differing: u32,
    /// Differing lines that read all zero.
    pub zero: u32,
    /// The first differing word: its byte offset, the word written and the
    /// word read.
    pub first: Option<(usize, u32, u32)>,
}

impl Seen {
    /// `read` against `written`, line by line.
    pub fn compare(written: &[u8], read: &[u8]) -> Self {
        let mut seen = Self::default();
        for (index, (written, read)) in written.chunks(LINE).zip(read.chunks(LINE)).enumerate() {
            seen.lines += 1;
            if written == read {
                continue;
            }
            seen.differing += 1;
            if read.iter().all(|&byte| byte == 0) {
                seen.zero += 1;
            }
            if seen.first.is_none() {
                let words = |bytes: &[u8]| {
                    bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|&word| u32::from_le_bytes(word))
                        .collect::<Vec<_>>()
                };
                let (written_words, read_words) = (words(written), words(read));
                if let Some(at) = written_words
                    .iter()
                    .zip(&read_words)
                    .position(|(written, read)| written != read)
                {
                    seen.first = Some((index * LINE + at * 4, written_words[at], read_words[at]));
                }
            }
        }
        seen
    }

    fn text(&self, reader: &str, writer: &str) -> String {
        let shown = self.lines - self.differing;
        let mut text = format!(
            "{reader} saw {shown} of {} {LINE}-byte lines {writer} wrote",
            self.lines
        );
        if let Some((offset, written, read)) = self.first {
            text.push_str(&format!(
                " (first differing word at +{offset:#x}: {written:#010x} written, \
                 {read:#010x} read; {} of the {} differing lines read zero)",
                self.zero, self.differing
            ));
        }
        text
    }
}

/// What [`check`] found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Check {
    /// The device has no memory the CPU maps that is the GPU's own, or none
    /// it maps that is not: nothing to compare.
    Untested(&'static str),
    /// The GPU's copy of the CPU's pattern, and the CPU's read of the GPU's
    /// fill.
    Compared { gpu: Seen, cpu: Seen },
    /// The copy did not signal within [`WAIT_NS`]: its memory is kept.
    Unfinished,
    /// An allocation, submission or read failed.
    Failed(Error),
}

impl Check {
    /// Whether the GPU missed any line the CPU wrote through its mapping.
    pub fn unseen(&self) -> bool {
        matches!(self, Self::Compared { gpu, .. } if gpu.differing != 0)
    }

    /// The boot log's line.
    pub fn line(&self) -> String {
        match self {
            Self::Untested(reason) => format!("mapped device memory not checked: {reason}"),
            Self::Compared { gpu, cpu } => format!(
                "mapped device memory: the {}; the {}",
                gpu.text("GPU", "the CPU"),
                cpu.text("CPU", "the GPU")
            ),
            Self::Unfinished => format!(
                "mapped device memory not checked: the GPU's copy did not finish within {} s",
                WAIT_NS / 1_000_000_000
            ),
            Self::Failed(error) => format!("mapped device memory not checked: {error:?}"),
        }
    }
}

/// The backend calls a check makes, which every [`Backend`] answers.
pub trait Probe {
    fn memory(&self) -> &physical::Memory;
    fn allocate_memory(&mut self, bytes: u64, request: MemoryRequest, types: u32)
    -> Result<Memory>;
    fn create_placed_buffer(&mut self, info: &BufferInfo, memory: Memory) -> Result<Buffer>;
    fn write_memory(&mut self, memory: Memory, offset: u64, bytes: &[u8]) -> Result<()>;
    fn read_memory(&mut self, memory: Memory, offset: u64, bytes: &mut [u8]) -> Result<()>;
    fn create_semaphore(&mut self) -> Result<Semaphore>;
    fn submit(&mut self, segment: &Segment) -> Result<()>;
    fn wait_semaphore(&mut self, semaphore: Semaphore, value: u64) -> Result<bool>;
    fn submit_failure(&mut self, semaphore: Semaphore, value: u64) -> Option<Error>;
    fn destroy(&mut self, object: Object);
}

impl<B: Backend> Probe for B {
    fn memory(&self) -> &physical::Memory {
        &self.physical_device().memory
    }
    fn allocate_memory(
        &mut self,
        bytes: u64,
        request: MemoryRequest,
        types: u32,
    ) -> Result<Memory> {
        Backend::allocate_memory(self, bytes, request, types)
    }
    fn create_placed_buffer(&mut self, info: &BufferInfo, memory: Memory) -> Result<Buffer> {
        Backend::create_placed_buffer(self, info, memory, 0).map(|(buffer, _)| buffer)
    }
    fn write_memory(&mut self, memory: Memory, offset: u64, bytes: &[u8]) -> Result<()> {
        Backend::write_memory(self, memory, offset, bytes)
    }
    fn read_memory(&mut self, memory: Memory, offset: u64, bytes: &mut [u8]) -> Result<()> {
        Backend::read_memory(self, memory, offset, bytes)
    }
    fn create_semaphore(&mut self) -> Result<Semaphore> {
        Backend::create_semaphore(self, 0)
    }
    fn submit(&mut self, segment: &Segment) -> Result<()> {
        Backend::submit(self, core::slice::from_ref(segment))
    }
    fn wait_semaphore(&mut self, semaphore: Semaphore, value: u64) -> Result<bool> {
        Backend::wait_semaphore(self, semaphore, value, WAIT_NS)
    }
    fn submit_failure(&mut self, semaphore: Semaphore, value: u64) -> Option<Error> {
        Backend::submit_failure(self, semaphore, value)
    }
    fn destroy(&mut self, object: Object) {
        Backend::destroy(self, object);
    }
}

/// The memory type bits that hold all of `with` and none of `without`.
pub fn types(memory: &physical::Memory, with: MemoryProperties, without: MemoryProperties) -> u32 {
    memory
        .types
        .iter()
        .enumerate()
        .take(32)
        .filter(|(_, kind)| kind.properties.contains(with) && !kind.properties.intersects(without))
        .fold(0, |bits, (index, _)| bits | 1 << index)
}

/// Writes [`pattern`] into mapped device-local memory, has the GPU copy it
/// into mapped system memory and fill a second device-local range with
/// [`FILL`], and compares what each side read.
pub fn check<P: Probe>(backend: &mut P) -> Check {
    let memory = backend.memory();
    let mapped = MemoryProperties::DEVICE_LOCAL.union(MemoryProperties::HOST_VISIBLE);
    let local = types(memory, mapped, MemoryProperties::EMPTY);
    let system = types(
        memory,
        MemoryProperties::HOST_VISIBLE,
        MemoryProperties::DEVICE_LOCAL,
    );
    if local == 0 {
        return Check::Untested("the CPU maps none of the GPU's own memory");
    }
    if system == 0 {
        return Check::Untested("the CPU maps no system memory of the GPU's");
    }
    let mut made = Made::default();
    let checked = run(backend, local, system, &mut made);
    if !matches!(checked, Check::Unfinished) {
        made.release(backend);
    }
    checked
}

/// What a check created, released after it unless the GPU may still write.
#[derive(Default)]
struct Made {
    memories: Vec<Memory>,
    buffers: Vec<Buffer>,
    semaphore: Option<Semaphore>,
}

impl Made {
    fn release<P: Probe>(self, backend: &mut P) {
        for buffer in self.buffers {
            backend.destroy(Object::Buffer(buffer));
        }
        for memory in self.memories {
            backend.destroy(Object::Memory(memory));
        }
        if let Some(semaphore) = self.semaphore {
            backend.destroy(Object::Semaphore(semaphore));
        }
    }
}

/// Mapped memory of `bytes` in one of `types`, with a buffer over it whole.
fn allocate<P: Probe>(
    backend: &mut P,
    made: &mut Made,
    bytes: u64,
    required: MemoryProperties,
    types: u32,
) -> Result<(Memory, Buffer)> {
    let request = MemoryRequest {
        required,
        preferred: MemoryProperties::EMPTY,
        mapped: true,
    };
    let memory = backend.allocate_memory(bytes, request, types)?;
    made.memories.push(memory);
    let info = BufferInfo {
        size: bytes,
        usage: BufferUsage::TRANSFER_SRC.union(BufferUsage::TRANSFER_DST),
        flags: BufferCreate::EMPTY,
    };
    let buffer = backend.create_placed_buffer(&info, memory)?;
    made.buffers.push(buffer);
    Ok((memory, buffer))
}

fn run<P: Probe>(backend: &mut P, local: u32, system: u32, made: &mut Made) -> Check {
    match compare(backend, local, system, made) {
        Ok(Some(check)) => check,
        Ok(None) => Check::Unfinished,
        Err(error) => Check::Failed(error),
    }
}

/// The comparison, or `None` when the GPU did not finish in time.
fn compare<P: Probe>(
    backend: &mut P,
    local: u32,
    system: u32,
    made: &mut Made,
) -> Result<Option<Check>> {
    let mapped = MemoryProperties::DEVICE_LOCAL.union(MemoryProperties::HOST_VISIBLE);
    let (local_memory, local_buffer) = allocate(backend, made, 2 * BYTES, mapped, local)?;
    let (system_memory, system_buffer) =
        allocate(backend, made, BYTES, MemoryProperties::HOST_VISIBLE, system)?;
    let written = pattern(BYTES as usize);
    backend.write_memory(local_memory, 0, &written)?;
    let semaphore = backend.create_semaphore()?;
    made.semaphore = Some(semaphore);
    let segment = Segment {
        waits: Vec::new(),
        binds: Vec::new(),
        commands: commands(local_buffer, system_buffer),
        signals: vec![(semaphore, 1)],
    };
    backend.submit(&segment)?;
    if !backend.wait_semaphore(semaphore, 1)? {
        return Ok(None);
    }
    // A submission the kernel driver refused still signals; its failure is
    // taken by the signal.
    if let Some(error) = backend.submit_failure(semaphore, 1) {
        return Err(error);
    }
    let mut copied = vec![0; BYTES as usize];
    backend.read_memory(system_memory, 0, &mut copied)?;
    let mut filled = vec![0; BYTES as usize];
    backend.read_memory(local_memory, BYTES, &mut filled)?;
    let fill: Vec<u8> = (0..BYTES / 4).flat_map(|_| FILL.to_le_bytes()).collect();
    Ok(Some(Check::Compared {
        gpu: Seen::compare(&written, &copied),
        cpu: Seen::compare(&fill, &filled),
    }))
}

/// The GPU's part: the copy of the CPU's pattern into system memory, the
/// fill of the second range, and the barrier that makes both the host's.
pub fn commands(local: Buffer, system: Buffer) -> Vec<Command> {
    vec![
        Command::CopyBuffer {
            src: local,
            dst: system,
            region: BufferCopy {
                src_offset: 0,
                dst_offset: 0,
                size: BYTES,
            },
        },
        Command::FillBuffer {
            buffer: local,
            offset: BYTES,
            size: BYTES,
            data: FILL,
        },
        Command::Barrier(Barrier {
            src_stages: PipelineStages::ALL_TRANSFER,
            src_access: Access::TRANSFER_WRITE,
            dst_stages: PipelineStages::HOST,
            dst_access: Access::HOST_READ,
        }),
    ]
}

#[cfg(test)]
mod tests;
