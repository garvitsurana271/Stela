//! The images an ahead-of-time translation covers, read from the files
//! macOS boots: a kernel collection (an `MH_FILESET` Mach-O whose entries
//! are the kernel and its kexts) and a dyld shared cache (its main file and
//! subcaches). Each image gives its UUID, its executable ranges at their
//! unslid addresses, its function starts (`LC_FUNCTION_STARTS`) and its
//! text symbols. Nothing here is slid: the boot logs each file's slide
//! (`PES` UUID lines, `--profile`), and every address here is the file's.
use anyhow::{Context, Result, anyhow, bail};
use object::macho::{self, DyldCacheHeader, LinkeditDataCommand};
use object::read::macho::{DyldCache, MachOFile64};
use object::{Endianness, Object, ObjectSegment, ObjectSymbol, SymbolKind};
use std::path::{Path, PathBuf};

/// One executable range of an image.
#[derive(Debug, Clone)]
pub struct Code<'data> {
    pub address: u64,
    pub bytes: &'data [u8],
}

/// One image: a kext or the kernel of a collection, or a library of a
/// shared cache.
#[derive(Debug, Clone)]
pub struct Image<'data> {
    pub name: String,
    pub uuid: Option<[u8; 16]>,
    pub code: Vec<Code<'data>>,
    /// Ascending and unique.
    pub starts: Vec<u64>,
    /// Text symbols, ascending by address, one per address.
    pub symbols: Vec<(u64, String)>,
}

impl<'data> Image<'data> {
    /// The bytes at `address`, up to `length` of them within one range.
    pub fn bytes(&self, address: u64, length: usize) -> Option<&'data [u8]> {
        let code = self
            .code
            .iter()
            .find(|code| (code.address..code.address + code.bytes.len() as u64).contains(&address))?;
        let at = (address - code.address) as usize;
        Some(&code.bytes[at..(at + length).min(code.bytes.len())])
    }

    /// The symbol naming `address`, the last at or below it.
    pub fn symbol(&self, address: u64) -> Option<&str> {
        let at = self.symbols.partition_point(|(start, _)| *start <= address);
        self.symbols
            .get(at.checked_sub(1)?)
            .filter(|(start, _)| *start == address)
            .map(|(_, name)| name.as_str())
    }
}

/// The ULEB128 deltas of `LC_FUNCTION_STARTS` from `text`, to a zero.
pub fn decode_starts(text: u64, bytes: &[u8]) -> Vec<u64> {
    let mut starts = Vec::new();
    let mut address = text;
    let mut at = 0;
    while at < bytes.len() {
        let (mut delta, mut shift) = (0u64, 0u32);
        while let Some(&byte) = bytes.get(at) {
            at += 1;
            if shift < 64 {
                delta |= u64::from(byte & 0x7f) << shift;
            }
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        if delta == 0 {
            break;
        }
        address = address.wrapping_add(delta);
        starts.push(address);
    }
    starts
}

fn u32_at(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data
        .get(at..at + 4)
        .with_context(|| format!("a word at {at:#x} past the file's end"))?;
    Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}

fn u64_at(data: &[u8], at: usize) -> Result<u64> {
    let bytes = data
        .get(at..at + 8)
        .with_context(|| format!("a word at {at:#x} past the file's end"))?;
    Ok(u64::from_le_bytes(bytes.try_into().expect("eight bytes")))
}

/// A 64-bit Mach-O header's load commands, as an analysis needs them.
#[derive(Debug, Default)]
pub struct Header {
    pub filetype: u32,
    pub uuid: Option<[u8; 16]>,
    /// Name, address, size, file offset, file size, initial protection.
    pub segments: Vec<(String, u64, u64, u64, u64, u32)>,
    /// `LC_FUNCTION_STARTS`: data offset and size.
    pub function_starts: Option<(u32, u32)>,
    /// `LC_FILESET_ENTRY`: address, file offset, entry id.
    pub entries: Vec<(u64, u64, String)>,
    /// `LC_SYMTAB`: symbol offset, count, string offset, size.
    pub symtab: Option<(u32, u32, u32, u32)>,
}

/// The header at `at` in `data`, a little-endian 64-bit Mach-O.
pub fn header(data: &[u8], at: usize) -> Result<Header> {
    if u32_at(data, at)? != macho::MH_MAGIC_64 {
        bail!("no 64-bit little-endian Mach-O header at {at:#x}");
    }
    let mut header = Header {
        filetype: u32_at(data, at + 12)?,
        ..Header::default()
    };
    let count = u32_at(data, at + 16)?;
    let mut command = at + 32;
    for _ in 0..count {
        let cmd = u32_at(data, command)?;
        let size = u32_at(data, command + 4)? as usize;
        if size < 8 {
            bail!("a load command of {size} bytes at {command:#x}");
        }
        match cmd {
            macho::LC_SEGMENT_64 => {
                let name = data
                    .get(command + 8..command + 24)
                    .context("a segment name")?;
                let name = String::from_utf8_lossy(name)
                    .trim_end_matches('\0')
                    .to_owned();
                header.segments.push((
                    name,
                    u64_at(data, command + 24)?,
                    u64_at(data, command + 32)?,
                    u64_at(data, command + 40)?,
                    u64_at(data, command + 48)?,
                    u32_at(data, command + 60)?,
                ));
            }
            macho::LC_UUID => {
                let bytes = data.get(command + 8..command + 24).context("a UUID")?;
                header.uuid = Some(bytes.try_into().expect("sixteen bytes"));
            }
            macho::LC_FUNCTION_STARTS => {
                header.function_starts =
                    Some((u32_at(data, command + 8)?, u32_at(data, command + 12)?));
            }
            macho::LC_FILESET_ENTRY => {
                let address = u64_at(data, command + 8)?;
                let offset = u64_at(data, command + 16)?;
                let id = u32_at(data, command + 24)? as usize;
                let name = data
                    .get(command + id..command + size)
                    .context("a fileset entry id")?;
                let name = name.split(|&byte| byte == 0).next().unwrap_or_default();
                header.entries.push((
                    address,
                    offset,
                    String::from_utf8_lossy(name).into_owned(),
                ));
            }
            macho::LC_SYMTAB => {
                header.symtab = Some((
                    u32_at(data, command + 8)?,
                    u32_at(data, command + 12)?,
                    u32_at(data, command + 16)?,
                    u32_at(data, command + 20)?,
                ));
            }
            _ => {}
        }
        command += size;
    }
    Ok(header)
}

/// The image of the header at `at` in `data`, a Mach-O whose segments'
/// file offsets and linkedit offsets are offsets in `data` (a standalone
/// Mach-O, or an entry of a kernel collection).
pub fn macho_image<'data>(data: &'data [u8], at: usize, name: &str) -> Result<Image<'data>> {
    let header = header(data, at)?;
    let mut code = Vec::new();
    for (_, address, _, offset, size, protection) in &header.segments {
        if protection & macho::VM_PROT_EXECUTE != 0 && *size != 0 {
            let bytes = data
                .get(*offset as usize..(*offset + *size) as usize)
                .with_context(|| format!("{name}: a code segment past the file's end"))?;
            code.push(Code {
                address: *address,
                bytes,
            });
        }
    }
    let text = header
        .segments
        .iter()
        .find(|segment| segment.0 == "__TEXT")
        .map(|segment| segment.1);
    let mut starts = match (text, header.function_starts) {
        (Some(text), Some((offset, size))) => data
            .get(offset as usize..(offset + size) as usize)
            .map(|bytes| decode_starts(text, bytes))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    starts.sort_unstable();
    starts.dedup();
    let mut symbols = Vec::new();
    if let Some((symbols_at, count, strings_at, strings_size)) = header.symtab {
        let strings = data
            .get(strings_at as usize..(strings_at + strings_size) as usize)
            .unwrap_or_default();
        for index in 0..count as usize {
            let entry = symbols_at as usize + 16 * index;
            let Some(record) = data.get(entry..entry + 16) else {
                break;
            };
            let name_at = u32::from_le_bytes(record[0..4].try_into().expect("four")) as usize;
            let kind = record[4];
            let section = record[5];
            let value = u64::from_le_bytes(record[8..16].try_into().expect("eight"));
            // A defined symbol in a section, not a debugging entry.
            if kind & 0xe0 != 0 || kind & 0x0e != 0x0e || section == 0 {
                continue;
            }
            let name = strings.get(name_at..).unwrap_or_default();
            let name = name.split(|&byte| byte == 0).next().unwrap_or_default();
            if !name.is_empty()
                && code.iter().any(|code: &Code<'_>| {
                    (code.address..code.address + code.bytes.len() as u64).contains(&value)
                })
            {
                symbols.push((value, String::from_utf8_lossy(name).into_owned()));
            }
        }
    }
    symbols.sort();
    symbols.dedup_by_key(|symbol| symbol.0);
    Ok(Image {
        name: name.to_owned(),
        uuid: header.uuid,
        code,
        starts,
        symbols,
    })
}

/// The images of a kernel collection (each fileset entry) or of a single
/// Mach-O, and the file's own UUID.
pub fn kernel_images<'data>(data: &'data [u8]) -> Result<(Option<[u8; 16]>, Vec<Image<'data>>)> {
    let top = header(data, 0)?;
    if top.filetype != macho::MH_FILESET {
        return Ok((top.uuid, vec![macho_image(data, 0, "kernel")?]));
    }
    let mut images = Vec::new();
    for (_, offset, name) in &top.entries {
        images.push(macho_image(data, *offset as usize, name)?);
    }
    Ok((top.uuid, images))
}

/// A file's bytes, mapped where the host maps files: a shared cache's are
/// gigabytes.
pub struct Mapped {
    #[cfg(unix)]
    pointer: *mut libc::c_void,
    #[cfg(unix)]
    length: usize,
    #[cfg(not(unix))]
    bytes: Vec<u8>,
}

impl Mapped {
    #[cfg(unix)]
    pub fn open(path: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let length = usize::try_from(file.metadata()?.len())?;
        if length == 0 {
            return Ok(Self {
                pointer: core::ptr::null_mut(),
                length,
            });
        }
        // SAFETY: a private read-only mapping of a file opened for
        // reading, unmapped at drop; nothing writes through it.
        let pointer = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            bail!(
                "mapping {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(Self { pointer, length })
    }

    #[cfg(not(unix))]
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            bytes: std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
        })
    }
}

impl core::ops::Deref for Mapped {
    type Target = [u8];

    #[cfg(unix)]
    fn deref(&self) -> &[u8] {
        if self.length == 0 {
            return &[];
        }
        // SAFETY: mapped readable for `length` bytes until drop.
        unsafe { core::slice::from_raw_parts(self.pointer.cast::<u8>(), self.length) }
    }

    #[cfg(not(unix))]
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(unix)]
impl Drop for Mapped {
    fn drop(&mut self) {
        if self.length != 0 {
            // SAFETY: the mapping `open` made, unmapped once.
            unsafe { libc::munmap(self.pointer, self.length) };
        }
    }
}

/// A dyld shared cache's main file and its subcaches, mapped.
pub struct CacheFiles {
    pub main: Mapped,
    pub subcaches: Vec<Mapped>,
}

impl CacheFiles {
    /// The cache whose main file is `main`, its subcaches beside it.
    pub fn open(main: &Path) -> Result<Self> {
        let mapped = Mapped::open(main)?;
        let suffixes = DyldCache::<Endianness>::subcache_suffixes(&mapped[..])
            .map_err(|error| anyhow!("{}: {error}", main.display()))?;
        let mut subcaches = Vec::new();
        for suffix in suffixes {
            let path = PathBuf::from(format!("{}{suffix}", main.display()));
            subcaches.push(Mapped::open(&path)?);
        }
        Ok(Self {
            main: mapped,
            subcaches,
        })
    }

    /// The cache's UUID and its images.
    pub fn images(&self) -> Result<(Option<[u8; 16]>, Vec<Image<'_>>)> {
        let slices: Vec<&[u8]> = self.subcaches.iter().map(|sub| &sub[..]).collect();
        let cache = DyldCache::<Endianness>::parse(&self.main[..], &slices)
            .map_err(|error| anyhow!("the cache: {error}"))?;
        let uuid = DyldCacheHeader::<Endianness>::parse(&self.main[..])
            .ok()
            .map(|header| header.uuid);
        let mut images = Vec::new();
        for image in cache.images() {
            let (Ok(path), Ok(object::File::MachO64(file))) = (image.path(), image.parse_object())
            else {
                continue;
            };
            images.push(cache_image(&cache, &file, path));
        }
        Ok((uuid, images))
    }
}

fn cache_image<'data, R: object::ReadRef<'data>>(
    cache: &DyldCache<'data, Endianness, &'data [u8]>,
    file: &MachOFile64<'data, Endianness, R>,
    path: &str,
) -> Image<'data> {
    let endian = file.endian();
    let mut code = Vec::new();
    for segment in file.segments() {
        let executable = matches!(segment.flags(),
            object::SegmentFlags::MachO { initprot, .. } if initprot & macho::VM_PROT_EXECUTE != 0);
        if executable && segment.size() != 0 {
            // The segment's bytes, from the cache file that holds them.
            if let Some((data, offset)) = cache.data_and_offset_for_address(segment.address()) {
                let start = offset as usize;
                if let Some(bytes) = data.get(start..start + segment.size() as usize) {
                    code.push(Code {
                        address: segment.address(),
                        bytes,
                    });
                }
            }
        }
    }
    // An image's linkedit offsets are offsets in the cache file that holds
    // its __LINKEDIT.
    let linkedit = file
        .segments()
        .find(|segment| segment.name().ok().flatten() == Some("__LINKEDIT"))
        .and_then(|segment| cache.data_and_offset_for_address(segment.address()))
        .map(|(data, _)| data);
    let text = file
        .segments()
        .find(|segment| segment.name().ok().flatten() == Some("__TEXT"))
        .map(|segment| segment.address());
    let mut starts = Vec::new();
    if let (Some(text), Ok(mut commands)) = (text, file.macho_load_commands()) {
        while let Ok(Some(command)) = commands.next() {
            if command.cmd() != macho::LC_FUNCTION_STARTS {
                continue;
            }
            let Ok(data) = command.data::<LinkeditDataCommand<Endianness>>() else {
                continue;
            };
            let (offset, size) = (
                data.dataoff.get(endian) as usize,
                data.datasize.get(endian) as usize,
            );
            if let Some(bytes) = linkedit.and_then(|data| data.get(offset..offset + size)) {
                starts.extend(decode_starts(text, bytes));
            }
        }
    }
    starts.sort_unstable();
    starts.dedup();
    let mut symbols: Vec<(u64, String)> = file
        .symbols()
        .filter(|symbol| symbol.kind() == SymbolKind::Text && symbol.is_definition())
        .filter_map(|symbol| Some((symbol.address(), symbol.name().ok()?.to_owned())))
        .filter(|(_, name)| !name.is_empty())
        .collect();
    symbols.sort();
    symbols.dedup_by_key(|symbol| symbol.0);
    Image {
        name: path.to_owned(),
        uuid: file.mach_uuid().ok().flatten(),
        code,
        starts,
        symbols,
    }
}

/// A UUID as the boot logs it.
pub fn uuid_text(uuid: &[u8; 16]) -> String {
    let hex: String = uuid.iter().map(|byte| format!("{byte:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal Mach-O: one `__TEXT` segment holding `code` at
    /// `0x1_0000_0000 + 0x1000`, a UUID and function starts.
    pub(crate) fn synthetic(code: &[u32], starts: &[u64]) -> Vec<u8> {
        let base = 0x1_0000_0000u64;
        let mut leb = Vec::new();
        let mut previous = base;
        for &start in starts {
            let mut delta = start - previous;
            previous = start;
            loop {
                let byte = (delta & 0x7f) as u8;
                delta >>= 7;
                if delta == 0 {
                    leb.push(byte);
                    break;
                }
                leb.push(byte | 0x80);
            }
        }
        leb.push(0);
        let commands = 72 + 24 + 16;
        let mut file = vec![0u8; 0x1000];
        let put32 = |file: &mut Vec<u8>, at: usize, value: u32| {
            file[at..at + 4].copy_from_slice(&value.to_le_bytes())
        };
        let put64 = |file: &mut Vec<u8>, at: usize, value: u64| {
            file[at..at + 8].copy_from_slice(&value.to_le_bytes())
        };
        put32(&mut file, 0, macho::MH_MAGIC_64);
        put32(&mut file, 4, macho::CPU_TYPE_ARM64);
        put32(&mut file, 12, macho::MH_EXECUTE);
        put32(&mut file, 16, 3);
        put32(&mut file, 20, commands);
        // LC_SEGMENT_64 __TEXT: the header page and the code page.
        let at = 32;
        put32(&mut file, at, macho::LC_SEGMENT_64);
        put32(&mut file, at + 4, 72);
        file[at + 8..at + 14].copy_from_slice(b"__TEXT");
        put64(&mut file, at + 24, base);
        put64(&mut file, at + 32, 0x2000);
        put64(&mut file, at + 40, 0);
        put64(&mut file, at + 48, 0x2000);
        put32(&mut file, at + 56, 5);
        put32(&mut file, at + 60, 5);
        // LC_UUID.
        let at = at + 72;
        put32(&mut file, at, macho::LC_UUID);
        put32(&mut file, at + 4, 24);
        for (index, byte) in file[at + 8..at + 24].iter_mut().enumerate() {
            *byte = index as u8;
        }
        // LC_FUNCTION_STARTS, its data after the code.
        let at = at + 24;
        put32(&mut file, at, macho::LC_FUNCTION_STARTS);
        put32(&mut file, at + 4, 16);
        put32(&mut file, at + 8, 0x2000);
        put32(&mut file, at + 12, leb.len() as u32);
        file.resize(0x2000, 0);
        for (index, word) in code.iter().enumerate() {
            let at = 0x1000 + 4 * index;
            file[at..at + 4].copy_from_slice(&word.to_le_bytes());
        }
        file.extend_from_slice(&leb);
        file
    }

    #[test]
    fn a_synthetic_image_reads_back() {
        let file = synthetic(&[0xd503_201f, 0xd65f_03c0], &[0x1_0000_1000, 0x1_0000_1004]);
        let (uuid, images) = kernel_images(&file).unwrap();
        assert_eq!(uuid, Some(core::array::from_fn(|index| index as u8)));
        let image = &images[0];
        assert_eq!(image.starts, vec![0x1_0000_1000, 0x1_0000_1004]);
        assert_eq!(
            image.bytes(0x1_0000_1004, 4),
            Some(&0xd65f_03c0u32.to_le_bytes()[..])
        );
        assert_eq!(
            uuid_text(&uuid.unwrap()),
            "00010203-0405-0607-0809-0A0B0C0D0E0F"
        );
    }
}
