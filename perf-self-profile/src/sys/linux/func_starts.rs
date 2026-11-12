//! Sorted table of function entry addresses, for deciding whether two
//! addresses lie in the same function. Used to recover the immediate caller of
//! a frameless leaf; see [`crate::sys::fp_profiler::unwind`].
//!
//! A lookup is signal-safe (binary search over an immutable slice), but
//! building the table is not. Hence [`init`] for signal-handler consumers and
//! [`get_or_build`] for the perf backend, which reads it in caller context.
//!
//! Modules are `mmap`ed, not read: only the section headers and
//! `.symtab`/`.dynsym` are touched.

use std::sync::OnceLock;

/// Cap on recorded entries (~8 MiB of `u64`). A binary with more functions
/// than this degrades to "unknown", which the unwinder treats conservatively.
const MAX_ENTRIES: usize = 1 << 20;

static TABLE: OnceLock<FuncStarts> = OnceLock::new();

/// Function entry addresses for all executable, file-backed mappings,
/// translated to runtime addresses and sorted, with the text ranges they came
/// from.
pub(crate) struct FuncStarts {
    starts: Box<[u64]>,
    /// Executable ranges of the modules that contributed symbols, merged and
    /// sorted. Bounds [`Self::entry_of`]: without it, any address above the
    /// lowest start resolves to whichever function precedes it, so a
    /// PAC-signed or scratch `x30` would name a real function.
    text: Box<[(u64, u64)]>,
}

impl FuncStarts {
    /// Greatest recorded start `<= addr`, or `None` if `addr` lies outside the
    /// text of a module we have symbols for.
    ///
    /// An unsymbolized function inside that text resolves to the preceding
    /// symbol, so two addresses can look like one function. That is the safe
    /// direction: callers skip the insert.
    pub(crate) fn entry_of(&self, addr: u64) -> Option<u64> {
        if !self.in_text(addr) {
            return None;
        }
        match self.starts.binary_search(&addr) {
            Ok(i) => Some(self.starts[i]),
            Err(0) => None,
            Err(i) => Some(self.starts[i - 1]),
        }
    }

    /// Whether `addr` is in a recorded executable range. Signal-safe.
    fn in_text(&self, addr: u64) -> bool {
        self.text
            .binary_search_by(|&(start, end)| {
                if addr < start {
                    std::cmp::Ordering::Greater
                } else if addr >= end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.starts.len()
    }

    /// Table whose text covers exactly `[min(starts), text_end)`.
    #[cfg(test)]
    pub(crate) fn from_sorted_for_test(starts: Vec<u64>, text_end: u64) -> Self {
        let text = match starts.first() {
            Some(&lo) => vec![(lo, text_end)],
            None => Vec::new(),
        };
        Self {
            starts: starts.into_boxed_slice(),
            text: text.into_boxed_slice(),
        }
    }
}

/// The leaf-caller rule, shared by both stack paths so they cannot drift.
///
/// Reports whether `lr` holds the immediate caller that a frame-pointer walk
/// dropped. Requires:
///
/// 1. `lr` differs from `chain_ret`, else the chain already reports the caller
///    and inserting would duplicate it. `None` means the chain ended at the
///    sampled PC, so there is nothing to duplicate.
/// 2. `lr` and `pc` in different functions. This is the discriminating test: a
///    framed function that already made a call leaves LR pointing into itself,
///    while a frameless leaf's LR points into its caller.
///
/// [`FuncStarts::entry_of`] supplies the plausibility check: it returns `None`
/// outside known text, which covers a null, scratch, PAC-signed or kernel `lr`.
/// Returns `false` without a table, and when both addresses resolve to one
/// function, so an unknown keeps chain-only behaviour.
///
/// `pc`, `lr` and `chain_ret` must be PAC-stripped, and `lr` and `chain_ret`
/// expressed the same way as each other (both return addresses, or both call
/// sites), or condition 1 cannot compare them.
pub(crate) fn lr_is_missing_caller(
    pc: u64,
    lr: u64,
    chain_ret: Option<u64>,
    table: Option<&FuncStarts>,
) -> bool {
    if chain_ret == Some(lr) {
        return false;
    }
    let Some(table) = table else {
        return false;
    };
    match (table.entry_of(pc), table.entry_of(lr)) {
        (Some(pc_fn), Some(lr_fn)) => pc_fn != lr_fn,
        _ => false,
    }
}

/// Build the table. Never call from a signal handler. Idempotent.
pub(crate) fn init() {
    TABLE.get_or_init(build);
}

/// The table, or `None` if not built yet. Signal-safe.
pub(crate) fn get() -> Option<&'static FuncStarts> {
    TABLE.get()
}

/// The table, building it on first use. Allocates, so caller context only;
/// use [`get`] from a signal handler.
pub(crate) fn get_or_build() -> &'static FuncStarts {
    TABLE.get_or_init(build)
}

fn build() -> FuncStarts {
    let mut starts = Vec::new();
    let mut text = Vec::new();
    let mut rejected = 0usize;
    let modules = exec_mappings();
    let module_count = modules.len();
    // One snapshot for the whole build: the loader lock is taken once, and
    // every module is judged against the same view of what is mapped.
    let loaded = LoadedImages::snapshot();
    for module in modules {
        let before = starts.len();
        match Mmap::open(&module.path) {
            Some(m) if matches_loaded_image(m.as_slice(), &module.bias_from, &loaded) => {
                collect_func_starts(m.as_slice(), &module.bias_from, &mut starts)
            }
            Some(_) => {
                rejected += 1;
                tracing::debug!("func_starts: {} is not the loaded image", module.path);
            }
            // Absent functions are handled conservatively by the caller.
            None => {
                rejected += 1;
                tracing::debug!("func_starts: cannot map {}", module.path);
            }
        }
        // Only bound against modules we actually got symbols for. A module we
        // could not read must stay outside the text, or its addresses would
        // resolve to the preceding module's last function.
        if starts.len() > before {
            text.extend(module.ranges);
        }
        if starts.len() >= MAX_ENTRIES {
            tracing::warn!(
                "func_starts: hit {MAX_ENTRIES} entries; leaf-caller recovery \
                 will be incomplete"
            );
            break;
        }
    }
    starts.sort_unstable();
    starts.dedup();
    let text = merge_ranges(text);
    // An empty table disables leaf-caller recovery for the whole process. That
    // is the safe direction, but it must not be invisible.
    if starts.is_empty() && module_count > 0 {
        tracing::warn!(
            "func_starts: no symbols from any of {module_count} modules \
             ({rejected} unreadable or not the loaded image); leaf-caller \
             recovery is disabled"
        );
    }
    tracing::debug!(
        "func_starts: {} function entries in {} text ranges",
        starts.len(),
        text.len()
    );
    FuncStarts {
        starts: starts.into_boxed_slice(),
        text: text.into_boxed_slice(),
    }
}

/// Sort and coalesce overlapping or adjacent ranges, so `in_text` can binary
/// search them.
fn merge_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// Read-only `mmap` of a file, unmapped on drop.
struct Mmap {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Mmap {
    /// Map `path`. `O_NONBLOCK` keeps a FIFO left at that path from blocking
    /// the build, and `S_ISREG` rejects anything that is not a plain file.
    ///
    /// This does not prove the file is the loaded image; see
    /// [`matches_loaded_image`], which the caller applies to the result.
    fn open(path: &str) -> Option<Self> {
        let cpath = std::ffi::CString::new(path).ok()?;
        // SAFETY: `cpath` is a valid NUL-terminated path; O_CLOEXEC avoids
        // leaking the fd across exec.
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` is open; `st` is written only on success.
        let len = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            let rc = libc::fstat(fd, &mut st);
            let regular = st.st_mode & libc::S_IFMT == libc::S_IFREG;
            if rc < 0 || st.st_size <= 0 || !regular {
                libc::close(fd);
                return None;
            }
            st.st_size as usize
        };
        // SAFETY: mapping `len` bytes of an open regular file, read-only.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                fd,
                0,
            )
        };
        // The mapping keeps the file alive.
        // SAFETY: `fd` was opened above and is not used again.
        unsafe { libc::close(fd) };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        Some(Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is a valid read-only mapping of `len` bytes, live for
        // the lifetime of `self`.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the mapping created in `open`.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// Executable, file-backed mappings grouped by module.
///
/// Anonymous executable mappings (JIT, trampolines) are skipped: we have no
/// symbols for them, so they must stay outside the text bound.
fn exec_mappings() -> Vec<Module> {
    let content = match std::fs::read_to_string("/proc/self/maps") {
        Ok(content) => content,
        Err(e) => {
            tracing::warn!(
                "func_starts: cannot read /proc/self/maps ({e}); leaf-caller recovery disabled"
            );
            return Vec::new();
        }
    };
    let mut out: Vec<Module> = Vec::new();
    for entry in content.lines().filter_map(parse_exec_maps_line) {
        match out.iter_mut().find(|m| m.path == entry.path) {
            Some(module) => module.ranges.push((entry.start, entry.end)),
            // The first executable mapping is enough for the load bias.
            None => out.push(Module {
                path: entry.path.to_string(),
                bias_from: Mapping {
                    start: entry.start,
                    offset: entry.offset,
                    len: entry.end.saturating_sub(entry.start),
                },
                ranges: vec![(entry.start, entry.end)],
            }),
        }
    }
    out
}

/// One executable, file-backed line of `/proc/self/maps`.
struct MapsEntry<'a> {
    start: u64,
    end: u64,
    offset: u64,
    path: &'a str,
}

/// Parse one `/proc/self/maps` line, keeping only executable file-backed ones.
///
/// The six fields are range, perms, offset, dev, inode, path. The path is the
/// *rest of the line*, not a whitespace-delimited token: it can contain spaces,
/// and a replaced file has `" (deleted)"` appended.
fn parse_exec_maps_line(line: &str) -> Option<MapsEntry<'_>> {
    let mut parts = line.splitn(6, ' ').filter(|f| !f.is_empty());
    // range, perms, offset, then dev and inode, which nothing here needs.
    let (range, perms, offset) = (parts.next()?, parts.next()?, parts.next()?);
    let (_dev, _inode) = (parts.next()?, parts.next()?);
    // Readable as well as executable: `matches_loaded_image` reads the live
    // bytes, and an execute-only mapping has no symbols we could trust anyway.
    if !perms.contains('x') || !perms.contains('r') {
        return None;
    }
    let path = parts.next()?.trim();
    // Anonymous mappings have no path; a deleted file's symbols no longer
    // describe what is mapped.
    if !path.starts_with('/') || path.ends_with("(deleted)") {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    Some(MapsEntry {
        start: u64::from_str_radix(start, 16).ok()?,
        end: u64::from_str_radix(end, 16).ok()?,
        offset: u64::from_str_radix(offset, 16).ok()?,
        path,
    })
}

/// One module's executable mappings.
struct Module {
    path: String,
    bias_from: Mapping,
    ranges: Vec<(u64, u64)>,
}

#[derive(Clone, Copy)]
struct Mapping {
    start: u64,
    offset: u64,
    /// Bytes mapped at `start`, bounding how much of the live image is readable.
    len: u64,
}

/// The page size, for mapping a segment's runtime range onto what the kernel
/// actually mapped. aarch64 is not always 4 KiB.
fn page_size() -> u64 {
    static PAGE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    // SAFETY: `sysconf` takes no pointers and cannot fail for `_SC_PAGESIZE`.
    *PAGE.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 => n as u64,
        // `sysconf` is not allowed to fail here; if it somehow does, the
        // smallest page any supported target uses keeps every range we derive
        // below a subset of what is mapped.
        _ => 4096,
    })
}

/// One loaded object, as the dynamic loader describes it.
struct LoadedImage {
    /// Runtime ranges of its readable `PT_LOAD` segments, page-rounded the way
    /// the kernel maps them.
    ranges: Vec<(u64, u64)>,
    /// Its `NT_GNU_BUILD_ID`, copied out of memory.
    build_id: Option<Vec<u8>>,
    /// The page-aligned start of its first executable segment, and the bytes
    /// there. That address is what `/proc/self/maps` reports for the mapping,
    /// so it is the key a maps entry matches on.
    window_at: u64,
    window: [u8; WINDOW],
}

/// What the dynamic loader has mapped, snapshotted under its own lock.
///
/// `dl_iterate_phdr` holds the loader lock for the duration of each callback,
/// so bytes copied out inside one cannot be unmapped by a concurrent `dlclose`
/// mid-read. That is what makes a plain dereference safe here, which matters
/// because neither way of reading another mapping out-of-band survives a
/// hardened process:
///
/// * `/proc/self/mem` is mode 0600, and the kernel reassigns `/proc/self/*` to
///   root once a process is not dumpable — a `setuid`/`setgid` drop under the
///   default `fs.suid_dumpable=0`, a file capability, or an explicit
///   `PR_SET_DUMPABLE 0`. `open` then fails `EACCES` before the kernel's
///   same-thread-group exemption is ever consulted, so every module is
///   rejected and recovery turns off process-wide.
/// * `process_vm_readv` is a debug-class syscall, and systemd's
///   `SystemCallFilter=@system-service` answers it with SIGSYS *kill* rather
///   than an error.
///
/// A dereference also cannot be made recoverable: `safe_load`'s handler only
/// fixes up faults whose PC lies inside its own instruction window. Hence the
/// loader lock rather than a fault-tolerant read.
struct LoadedImages(Vec<LoadedImage>);

impl LoadedImages {
    fn snapshot() -> Self {
        let mut images: Vec<LoadedImage> = Vec::new();
        // SAFETY: `snapshot_image` is `extern "C"` with the signature
        // `dl_iterate_phdr` expects, and `data` is the `Vec` it casts back.
        unsafe {
            libc::dl_iterate_phdr(
                Some(snapshot_image),
                (&raw mut images).cast::<libc::c_void>(),
            );
        }
        Self(images)
    }

    /// The object covering `addr`.
    ///
    /// `None` means no loaded object covers it, which is not the same as an
    /// object with no build-id: the first has no evidence either way, the
    /// second is evidence that the file should not have one.
    fn covering(&self, addr: u64) -> Option<&LoadedImage> {
        self.0
            .iter()
            .find(|img| img.ranges.iter().any(|&(lo, hi)| addr >= lo && addr < hi))
    }
}

extern "C" fn snapshot_image(
    info: *mut libc::dl_phdr_info,
    _size: libc::size_t,
    data: *mut libc::c_void,
) -> libc::c_int {
    // SAFETY: the loader passes an `info` valid for this call and the `data`
    // pointer given to `dl_iterate_phdr`, and holds its lock throughout, so
    // every address derived from `info` stays mapped until we return.
    unsafe {
        let out = &mut *data.cast::<Vec<LoadedImage>>();
        let info = &*info;
        // Annotated rather than cast: `Elf64_Addr` is `u64` on every target
        // this compiles for, so a 32-bit one would be a type error here rather
        // than a silent truncation.
        let base: u64 = info.dlpi_addr;
        let phdrs = std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum as usize);

        let page = page_size();
        let mut ranges = Vec::new();
        for p in phdrs {
            // Readable as well as loaded: execute-only text is mapped but
            // dereferencing it faults.
            if p.p_type != PT_LOAD || p.p_flags & PF_R == 0 {
                continue;
            }
            let at = base.wrapping_add(p.p_vaddr);
            let lo = at & !(page - 1);
            // Page-round the end up: the kernel maps whole pages, so the tail
            // page of a segment is mapped even past `p_memsz`.
            let Some(hi) = at
                .checked_add(p.p_memsz)
                .and_then(|end| end.checked_add(page - 1))
                .map(|end| end & !(page - 1))
            else {
                continue;
            };
            if hi > lo {
                ranges.push((lo, hi));
            }
        }
        let readable = |addr: u64, len: usize| {
            ranges
                .iter()
                .any(|&(lo, hi)| addr >= lo && addr <= hi && hi - addr >= len as u64)
        };

        let mut build_id = None;
        for p in phdrs {
            if p.p_type != PT_NOTE {
                continue;
            }
            let at = base.wrapping_add(p.p_vaddr);
            let len = p.p_filesz as usize;
            // A `PT_NOTE` is not required to lie inside a `PT_LOAD`; if it does
            // not, its runtime address is not mapped at all.
            if !readable(at, len) {
                continue;
            }
            if let Some(id) = build_id_in_notes(std::slice::from_raw_parts(at as *const u8, len)) {
                build_id = Some(id.to_vec());
                break;
            }
        }

        let mut window_at = 0;
        let mut window = [0u8; WINDOW];
        for p in phdrs {
            if p.p_type != PT_LOAD || p.p_flags & PF_X == 0 {
                continue;
            }
            // The kernel rounds a mapping's start down to a page, so this is
            // the address maps reports for this segment.
            let at = base.wrapping_add(p.p_vaddr) & !(page - 1);
            if readable(at, WINDOW) {
                window_at = at;
                std::ptr::copy_nonoverlapping(at as *const u8, window.as_mut_ptr(), WINDOW);
            }
            break;
        }

        out.push(LoadedImage {
            ranges,
            build_id,
            window_at,
            window,
        });
    }
    0
}

/// The `NT_GNU_BUILD_ID` descriptor in a `PT_NOTE` segment's bytes.
///
/// The build-id is a hash of the linked image, so two differing builds of the
/// same library get differing ids — which the byte window cannot establish,
/// since it covers whatever the executable mapping starts with (the ELF
/// header, `.init` or `.plt`) rather than anything build-specific.
fn build_id_in_notes(notes: &[u8]) -> Option<&[u8]> {
    // Notes are `namesz, descsz, type, name[namesz], desc[descsz]`, each of
    // name and desc padded to 4 bytes.
    let mut at = 0usize;
    while at + 12 <= notes.len() {
        let namesz = rd_u32(notes, at)? as usize;
        let descsz = rd_u32(notes, at + 4)? as usize;
        let kind = rd_u32(notes, at + 8)?;
        let name_at = at.checked_add(12)?;
        let desc_at = name_at.checked_add(namesz.checked_add(3)? & !3)?;
        let next = desc_at.checked_add(descsz.checked_add(3)? & !3)?;
        if next > notes.len() {
            break;
        }
        if kind == NT_GNU_BUILD_ID
            && notes.get(name_at..name_at + namesz) == Some(b"GNU\0".as_slice())
            && descsz > 0
        {
            return notes.get(desc_at..desc_at + descsz);
        }
        at = next;
    }
    None
}

/// A file's `NT_GNU_BUILD_ID`, read straight out of its `PT_NOTE` contents.
fn build_id_from_file(file: &[u8]) -> Option<&[u8]> {
    for ph in program_headers(file)? {
        if rd_u32(file, ph)? != PT_NOTE {
            continue;
        }
        let seg_off = rd_u64(file, ph + 8).and_then(as_usize)?;
        let seg_len = rd_u64(file, ph + 32).and_then(as_usize)?;
        let Some(notes) = file.get(seg_off..seg_off.checked_add(seg_len)?) else {
            continue;
        };
        if let Some(id) = build_id_in_notes(notes) {
            return Some(id);
        }
    }
    None
}

/// Byte offsets of each program header, bounds-checked against the file.
fn program_headers(file: &[u8]) -> Option<impl Iterator<Item = usize> + '_> {
    let phoff = rd_u64(file, 32).and_then(as_usize)?;
    let phentsize = rd_u16(file, 54)? as usize;
    let phnum = rd_u16(file, 56)? as usize;
    if phentsize < PROGRAM_HEADER_SIZE || !fits(file, phoff, phnum.checked_mul(phentsize)) {
        return None;
    }
    Some((0..phnum).map_while(move |i| phoff.checked_add(i.checked_mul(phentsize)?)))
}

/// Whether the file just opened is the image actually loaded at this mapping.
///
/// Re-opening a module by name races with anything that replaces it — a package
/// upgrade over a loaded library — and pairing the new file's symbols with the
/// old mapping's load bias puts wrong addresses in the table.
///
/// Identity comes from the bytes, not from `(dev, inode)`: `/proc/self/maps`
/// and `fstat` report different devices on overlayfs before kernel ~6.8 and on
/// btrfs subvolumes, so comparing them rejects every module. See
/// `docs/design/leaf-caller-recovery.md`.
///
/// The live side comes from [`LoadedImages`], so a module the dynamic loader
/// does not know about is rejected: there is no trustworthy way to read its
/// bytes, and no trustworthy bias for it either. The GNU build-id decides when
/// either side has one. Otherwise this falls back to comparing the first bytes
/// of the executable mapping, which only establishes that the file is *a* build
/// with that prologue. A module whose text is relocated (`DT_TEXTREL`) fails
/// the fallback and is skipped.
fn matches_loaded_image(file: &[u8], m: &Mapping, loaded: &LoadedImages) -> bool {
    let Some(live) = loaded.covering(m.start) else {
        return false;
    };
    let Some(e_type) = rd_u16(file, 16) else {
        return false;
    };
    if load_bias(file, m, e_type).is_none() {
        return false;
    }

    match (build_id_from_file(file), live.build_id.as_deref()) {
        (Some(from_file), Some(from_memory)) => return from_file == from_memory,
        (None, None) => {}
        // One side has an id and the other does not, so they are not the same
        // link of the same source.
        _ => return false,
    }

    if live.window_at != m.start {
        return false;
    }
    let Ok(mapped) = usize::try_from(m.len) else {
        return false;
    };
    let Ok(start) = usize::try_from(m.offset) else {
        return false;
    };
    let n = WINDOW.min(mapped);
    if n == 0 {
        return false;
    }
    let Some(from_file) = file.get(start..start.saturating_add(n)) else {
        return false;
    };
    from_file == &live.window[..n]
}
// ELF64 constants, parsed by fixed offset rather than via `blazesym` (already a
// dependency): measured far cheaper, and blazesym's ELF source defaults to
// consulting debug info, which reports no symbols at all for a module without
// DWARF such as a distro glibc. Numbers in
// `docs/design/leaf-caller-recovery.md#why-the-symbol-parsing-is-hand-written`.
const ELF_MAGIC: &[u8] = b"\x7fELF";
const ELFCLASS64: u8 = 2;
const ET_EXEC: u16 = 2;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const NT_GNU_BUILD_ID: u32 = 3;
const PF_X: u32 = 1;
const PF_R: u32 = 4;
const SHT_SYMTAB: u32 = 2;
const SHT_DYNSYM: u32 = 11;
const STT_FUNC: u8 = 2;
const SYM_ENTSIZE: usize = 24;
/// Bytes of the executable mapping compared when a module has no build-id.
const WINDOW: usize = 64;
const SECTION_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;

// Every offset below is derived from the file's own header fields, so each read
// is bounds-checked and each offset built with checked arithmetic: a corrupt or
// hostile module must be skipped, not trusted. The loader does not validate
// section headers, and this runs during library init and on the sample-drain
// path, where a panic or a long loop takes the host process with it.
fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(off..off.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(off..off.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(off..off.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Append runtime function entry addresses from one ELF module.
fn collect_func_starts(b: &[u8], m: &Mapping, out: &mut Vec<u64>) {
    if b.get(..4) != Some(ELF_MAGIC) || b.get(4) != Some(&ELFCLASS64) {
        return;
    }
    let Some(e_type) = rd_u16(b, 16) else { return };
    let Some(bias) = load_bias(b, m, e_type) else {
        return;
    };

    let Some(shoff) = rd_u64(b, 40).and_then(as_usize) else {
        return;
    };
    let Some(shentsize) = rd_u16(b, 58) else {
        return;
    };
    let Some(shnum) = rd_u16(b, 60) else { return };
    let shentsize = shentsize as usize;
    // A short `shentsize` would make every iteration re-read one header, and
    // zero would do it `shnum` times (up to 65535) with the inner symbol loop
    // running in full each time. Bounding each loop separately is not enough:
    // their product is the work.
    if shentsize < SECTION_HEADER_SIZE || !fits(b, shoff, (shnum as usize).checked_mul(shentsize)) {
        return;
    }
    // Total symbol reads per module, independent of what the headers claim.
    let mut budget = b.len() / SYM_ENTSIZE;

    for i in 0..shnum as usize {
        let Some(sh) = i.checked_mul(shentsize).and_then(|o| shoff.checked_add(o)) else {
            return;
        };
        let Some(sh_type) = rd_u32(b, sh + 4) else {
            continue;
        };
        if sh_type != SHT_SYMTAB && sh_type != SHT_DYNSYM {
            continue;
        }
        let (Some(sh_offset), Some(sh_size), Some(sh_entsize)) = (
            rd_u64(b, sh + 24).and_then(as_usize),
            rd_u64(b, sh + 32).and_then(as_usize),
            rd_u64(b, sh + 56).and_then(as_usize),
        ) else {
            continue;
        };
        let entsize = if sh_entsize == 0 {
            SYM_ENTSIZE
        } else {
            sh_entsize
        };
        // Reject a section that does not lie inside the file: a bogus `sh_size`
        // would otherwise drive `sh_size / entsize` iterations that read
        // nothing.
        if entsize < SYM_ENTSIZE || !fits(b, sh_offset, Some(sh_size)) {
            continue;
        }
        for j in 0..(sh_size / entsize) {
            if budget == 0 {
                tracing::warn!("func_starts: symbol budget exhausted; table is incomplete");
                return;
            }
            budget -= 1;
            let Some(sym) = j
                .checked_mul(entsize)
                .and_then(|o| sh_offset.checked_add(o))
            else {
                break;
            };
            let Some(st_info) = b.get(sym + 4) else {
                continue;
            };
            if st_info & 0xf != STT_FUNC {
                continue;
            }
            let Some(st_value) = rd_u64(b, sym + 8) else {
                continue;
            };
            if st_value == 0 {
                continue; // undefined / imported
            }
            out.push(st_value.wrapping_add(bias));
            if out.len() >= MAX_ENTRIES {
                return;
            }
        }
    }
}

/// A file-supplied `u64` as an in-memory offset, or `None` if it cannot be one.
/// On 32-bit targets `as usize` would truncate instead.
fn as_usize(v: u64) -> Option<usize> {
    usize::try_from(v).ok()
}

/// Whether `[off, off + len)` lies inside `b`.
fn fits(b: &[u8], off: usize, len: Option<usize>) -> bool {
    len.and_then(|len| off.checked_add(len))
        .is_some_and(|end| end <= b.len())
}

/// Difference between runtime and link-time addresses for this module.
///
/// `ET_EXEC` is loaded at its link-time addresses, so the bias is zero. For
/// `ET_DYN` (PIE and shared objects) it comes from the executable `PT_LOAD`
/// segment covering the mapping's file offset.
///
/// Only `PF_X` segments are considered, and the comparison is against the
/// segment's page-aligned start. `m` describes an executable mapping, and the
/// kernel rounds a mapping's file offset down to a page. With `-z
/// noseparate-code` (the lld and mold default) the executable segment's
/// `p_offset` is not page-aligned, so that rounded-down offset can fall inside
/// the *preceding* read-only segment; matching it would shift every symbol in
/// the module by that segment's `p_vaddr - p_offset`.
fn load_bias(b: &[u8], m: &Mapping, e_type: u16) -> Option<u64> {
    if e_type == ET_EXEC {
        return Some(0);
    }
    for ph in program_headers(b)? {
        if rd_u32(b, ph)? != PT_LOAD || rd_u32(b, ph + 4)? & PF_X == 0 {
            continue;
        }
        let p_offset = rd_u64(b, ph + 8)?;
        let p_vaddr = rd_u64(b, ph + 16)?;
        let p_filesz = rd_u64(b, ph + 32)?;
        let p_align = rd_u64(b, ph + 48)?.max(1);
        let seg_start = p_offset & !(p_align - 1);
        if m.offset >= seg_start && m.offset < p_offset.saturating_add(p_filesz) {
            // runtime = p_vaddr + (offset - p_offset) + bias
            return Some(
                m.start
                    .wrapping_sub(p_vaddr)
                    .wrapping_sub(m.offset.wrapping_sub(p_offset)),
            );
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn entry_of_finds_containing_function() {
        let t = FuncStarts::from_sorted_for_test(vec![0x1000, 0x2000, 0x3000], 0x4000);
        assert_eq!(t.entry_of(0x1000), Some(0x1000)); // exact start
        assert_eq!(t.entry_of(0x1004), Some(0x1000)); // inside
        assert_eq!(t.entry_of(0x1fff), Some(0x1000)); // just before next
        assert_eq!(t.entry_of(0x2000), Some(0x2000));
        assert_eq!(t.entry_of(0x3fff), Some(0x3000)); // last function
    }

    #[test]
    fn entry_of_rejects_addresses_below_the_table() {
        let t = FuncStarts::from_sorted_for_test(vec![0x1000], 0x2000);
        assert_eq!(t.entry_of(0x0fff), None);
    }

    #[test]
    fn entry_of_on_empty_table_is_none() {
        let t = FuncStarts::from_sorted_for_test(vec![], 0x4000);
        assert_eq!(t.entry_of(0x1000), None);
    }

    /// The bound is what stops a non-text address from naming a real function:
    /// a scratch or PAC-signed `x30` must not resolve to the last entry.
    #[test]
    fn entry_of_rejects_addresses_past_the_text() {
        let t = FuncStarts::from_sorted_for_test(vec![0x1000, 0x2000, 0x3000], 0x4000);
        assert_eq!(t.entry_of(0x4000), None); // first byte past the text
        assert_eq!(t.entry_of(0x9999), None); // heap-like value
        assert_eq!(t.entry_of(0x00aa_0000_0000_3000), None); // PAC-signed
    }

    #[test]
    fn in_text_spans_every_recorded_range() {
        let t = FuncStarts {
            starts: vec![0x1000, 0x8000].into_boxed_slice(),
            text: vec![(0x1000, 0x2000), (0x8000, 0x9000)].into_boxed_slice(),
        };
        assert_eq!(t.entry_of(0x1500), Some(0x1000));
        assert_eq!(t.entry_of(0x8500), Some(0x8000));
        // The gap between the two modules belongs to neither.
        assert_eq!(t.entry_of(0x5000), None);
    }

    #[test]
    fn merge_ranges_coalesces_and_sorts() {
        assert_eq!(
            merge_ranges(vec![(0x8000, 0x9000), (0x1000, 0x2000), (0x2000, 0x3000)]),
            vec![(0x1000, 0x3000), (0x8000, 0x9000)]
        );
        assert_eq!(
            merge_ranges(vec![(0x1000, 0x3000), (0x1800, 0x2000)]),
            vec![(0x1000, 0x3000)]
        );
        assert_eq!(merge_ranges(vec![]), vec![]);
    }

    /// Two addresses in different functions must resolve to different entries.
    #[test]
    fn distinguishes_adjacent_functions() {
        let t = FuncStarts::from_sorted_for_test(vec![0x1000, 0x1010], 0x2000);
        assert_ne!(t.entry_of(0x1008), t.entry_of(0x1014));
    }

    #[test]
    fn builds_a_non_empty_table_for_this_process() {
        init();
        let t = get().expect("initialised");
        // This test binary has symbols, so the table must be populated.
        assert!(t.len() > 0, "expected function entries for this process");
        // Our own address must resolve.
        // Equality, not just `is_some`: a uniformly wrong load bias would still
        // resolve this address, to the wrong function.
        let here = builds_a_non_empty_table_for_this_process as *const () as usize as u64;
        assert_eq!(t.entry_of(here), Some(here));
    }

    /// The table must still be built when the process is not dumpable. The
    /// kernel reassigns `/proc/self/*` to root as soon as that flag is cleared,
    /// so a module identity check that read `/proc/self/mem` rejected *every*
    /// module here and turned recovery off process-wide.
    ///
    /// The flag cannot be set back, and it is process-wide, so this re-runs
    /// itself as a child rather than clearing it for the rest of the suite.
    #[test]
    fn the_table_is_built_in_a_non_dumpable_process() {
        const CHILD: &str = "DIAL9_NON_DUMPABLE_CHILD";
        const NAME: &str = concat!(
            "sys::linux::func_starts::tests::",
            "the_table_is_built_in_a_non_dumpable_process"
        );

        if std::env::var_os(CHILD).is_none() {
            let exe = std::env::current_exe().expect("own test binary");
            let status = std::process::Command::new(exe)
                .args(["--exact", NAME, "--nocapture"])
                .env(CHILD, "1")
                .status()
                .expect("re-run this test as a child");
            assert!(status.success(), "see the child's output above");
            return;
        }

        // SAFETY: `prctl` with `PR_SET_DUMPABLE`/`PR_GET_DUMPABLE` passes no
        // pointers and affects only this child process.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_DUMPABLE, 0), 0, "PR_SET_DUMPABLE");
            assert_eq!(libc::prctl(libc::PR_GET_DUMPABLE), 0, "flag must be clear");
        }
        // Not an assertion: root may open it regardless, in which case the
        // table being built says less, but must still hold.
        if std::fs::File::open("/proc/self/mem").is_ok() {
            eprintln!("note: /proc/self/mem still opens here (running as root?)");
        }
        assert!(
            build().len() > 0,
            "a not-dumpable process must still get a table"
        );
    }

    /// One ELF note: `namesz, descsz, type, name, desc`, each of name and desc
    /// padded to 4 bytes.
    fn note(name: &[u8], kind: u32, desc: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(name.len() as u32).to_le_bytes());
        v.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(name);
        v.resize(v.len() + (name.len().wrapping_neg() & 3), 0);
        v.extend_from_slice(desc);
        v.resize(v.len() + (desc.len().wrapping_neg() & 3), 0);
        v
    }

    /// A loader snapshot holding one object, so the identity check can be
    /// driven without depending on this process's real mappings.
    fn loaded(window_at: u64, window: &[u8], build_id: Option<&[u8]>) -> LoadedImages {
        let mut w = [0u8; WINDOW];
        let n = window.len().min(WINDOW);
        w[..n].copy_from_slice(&window[..n]);
        LoadedImages(vec![LoadedImage {
            ranges: vec![(window_at, window_at + 0x1000)],
            build_id: build_id.map(<[u8]>::to_vec),
            window_at,
            window: w,
        }])
    }

    /// Where the test mappings live. Any address works: nothing is dereferenced
    /// any more, since the live bytes come from the loader snapshot.
    const MAP_AT: u64 = 0x5000_0000;

    /// The byte-window fallback, on an image with no build-id.
    #[test]
    fn identity_check_compares_file_against_memory() {
        let img = elf(ET_EXEC, &[], &[]);
        assert!(
            build_id_from_file(&img).is_none(),
            "fixture must have no build-id"
        );
        // Past the 64-byte ELF header, so a changed byte cannot alter a header
        // field and change the outcome for the wrong reason.
        let at = 64usize;
        assert!(
            img.len() > at + WINDOW,
            "fixture must extend past the compared window"
        );
        let m = Mapping {
            start: MAP_AT,
            offset: at as u64,
            len: WINDOW as u64,
        };
        let live = loaded(MAP_AT, &img[at..at + WINDOW], None);
        assert!(matches_loaded_image(&img, &m, &live));

        // One byte different inside the compared window.
        let mut changed = img.clone();
        changed[at + 3] ^= 0xff;
        assert!(!matches_loaded_image(&changed, &m, &live));

        // A byte past the window is not compared.
        let mut beyond = img.clone();
        beyond[at + WINDOW] ^= 0xff;
        assert!(matches_loaded_image(&beyond, &m, &live));

        // A mapping shorter than the window compares only `len` bytes.
        let short = Mapping { len: 4, ..m };
        let mut changed_late = img.clone();
        changed_late[at + 8] ^= 0xff;
        assert!(matches_loaded_image(&changed_late, &short, &live));
        assert!(!matches_loaded_image(&changed_late, &m, &live));

        // File too short to cover the mapping's offset.
        assert!(!matches_loaded_image(&img[..at], &m, &live));
    }

    /// A mapping no loaded object covers must be rejected. There is no
    /// trustworthy way to read its live bytes and no trustworthy bias for it,
    /// so "unknown to the loader" has to mean "skip", not "assume it matches".
    #[test]
    fn identity_check_rejects_a_mapping_the_loader_does_not_know() {
        let img = elf(ET_EXEC, &[], &[]);
        let at = 64usize;
        let m = Mapping {
            start: MAP_AT,
            offset: at as u64,
            len: WINDOW as u64,
        };
        // Control: the identical file and window pass when the loader covers
        // the mapping, so a rejection below is the coverage check and not
        // something else about the fixture.
        let known = loaded(MAP_AT, &img[at..at + WINDOW], None);
        assert!(matches_loaded_image(&img, &m, &known));

        let elsewhere = loaded(MAP_AT + 0x10000, &img[at..at + WINDOW], None);
        assert!(!matches_loaded_image(&img, &m, &elsewhere));
        assert!(!matches_loaded_image(&img, &m, &LoadedImages(Vec::new())));
    }

    /// `build_id_from_file` must find `NT_GNU_BUILD_ID` in a `PT_NOTE` segment.
    #[test]
    fn finds_the_gnu_build_id() {
        let id = [0xAAu8; 20];
        let n = note(b"GNU\0", NT_GNU_BUILD_ID, &id);
        // The note segment sits at the symbol area, past both header tables.
        let seg_off = sym_area(1, 0) as u64;
        let mut b = elf(
            ET_EXEC,
            &[(PT_NOTE, 0, seg_off, 0x9000, n.len() as u64, 1)],
            &[],
        );
        b.resize(seg_off as usize + n.len(), 0);
        b[seg_off as usize..].copy_from_slice(&n);
        assert_eq!(build_id_from_file(&b), Some(id.as_slice()));
    }

    /// Every guard in the note walk, one case per mutation it exists to catch.
    #[rstest]
    #[case::only_the_id(vec![note(b"GNU\0", NT_GNU_BUILD_ID, &[1, 2, 3, 4])], Some(vec![1, 2, 3, 4]))]
    // An empty descriptor is not an id: accepting it would make any two modules
    // carrying such a note compare equal on no bytes at all.
    #[case::empty_descriptor(vec![note(b"GNU\0", NT_GNU_BUILD_ID, &[])], None)]
    // Type 3 under another vendor's name.
    #[case::other_vendor(vec![note(b"Xen\0", NT_GNU_BUILD_ID, &[9; 8])], None)]
    // `.note.ABI-tag` (1) and `.note.gnu.property` (5) are GNU notes that are
    // identical across builds, so neither may stand in for an id.
    #[case::abi_tag(vec![note(b"GNU\0", 1, &[9; 8])], None)]
    #[case::gnu_property(vec![note(b"GNU\0", 5, &[9; 8])], None)]
    // The walk must advance past notes that precede the id, not stop at the
    // first entry in the segment.
    #[case::id_is_not_first(
        vec![note(b"GNU\0", 1, &[9; 16]), note(b"GNU\0", NT_GNU_BUILD_ID, &[7; 20])],
        Some(vec![7; 20])
    )]
    // A descriptor whose length is not a multiple of 4, so the next note starts
    // only if the padding is applied.
    #[case::unpadded_descriptor(
        vec![note(b"GNU\0", 1, &[9; 3]), note(b"GNU\0", NT_GNU_BUILD_ID, &[6; 20])],
        Some(vec![6; 20])
    )]
    // Longer than any real id; it is returned whole rather than clamped.
    #[case::long_descriptor(vec![note(b"GNU\0", NT_GNU_BUILD_ID, &[5; 80])], Some(vec![5; 80]))]
    fn walks_notes_for_the_build_id(#[case] notes: Vec<Vec<u8>>, #[case] want: Option<Vec<u8>>) {
        assert_eq!(build_id_in_notes(&notes.concat()).map(<[u8]>::to_vec), want);
    }

    /// A note running past the end of its segment stops the walk rather than
    /// reading beyond it.
    #[rstest]
    #[case(1)]
    #[case(13)]
    #[case(20)]
    fn a_truncated_note_is_not_an_id(#[case] cut_tail: usize) {
        let full = note(b"GNU\0", NT_GNU_BUILD_ID, &[3; 20]);
        assert_eq!(build_id_in_notes(&full[..full.len() - cut_tail]), None);
    }

    /// Sizes near `u32::MAX` must not wrap into an offset that looks in-bounds.
    #[rstest]
    #[case(u32::MAX, 20)]
    #[case(4, u32::MAX)]
    #[case(u32::MAX, u32::MAX)]
    fn note_sizes_near_the_limit_are_rejected(#[case] namesz: u32, #[case] descsz: u32) {
        let mut v = Vec::new();
        v.extend_from_slice(&namesz.to_le_bytes());
        v.extend_from_slice(&descsz.to_le_bytes());
        v.extend_from_slice(&NT_GNU_BUILD_ID.to_le_bytes());
        v.extend_from_slice(b"GNU\0");
        v.extend_from_slice(&[0xAA; 20]);
        assert_eq!(build_id_in_notes(&v), None);
    }

    /// The build-id must be what decides, not merely be available: in these
    /// fixtures the byte window matches every time, so only the build-id
    /// comparison can tell them apart.
    #[test]
    fn build_id_decides_when_the_byte_window_would_pass() {
        let id = [0xAAu8; 20];
        let n = note(b"GNU\0", NT_GNU_BUILD_ID, &id);
        let seg_off = sym_area(1, 0);
        let mut img = elf(
            ET_EXEC,
            &[(PT_NOTE, 0, seg_off as u64, 0x9000, n.len() as u64, 1)],
            &[],
        );
        img.resize(seg_off + n.len(), 0);
        img[seg_off..].copy_from_slice(&n);
        assert_eq!(build_id_from_file(&img), Some(id.as_slice()));

        let at = 64usize;
        let m = Mapping {
            start: MAP_AT,
            offset: at as u64,
            len: WINDOW as u64,
        };
        let window = img[at..at + WINDOW].to_vec();

        assert!(
            matches_loaded_image(&img, &m, &loaded(MAP_AT, &window, Some(&id))),
            "matching build-id must pass"
        );
        assert!(
            !matches_loaded_image(&img, &m, &loaded(MAP_AT, &window, Some(&[0xBBu8; 20]))),
            "a different build-id must reject the module"
        );
        // An id on one side only: the file is not the link that was loaded.
        assert!(
            !matches_loaded_image(&img, &m, &loaded(MAP_AT, &window, None)),
            "a file with an id must not pass against an image without one"
        );
        let plain = elf(ET_EXEC, &[], &[]);
        assert!(
            !matches_loaded_image(
                &plain,
                &m,
                &loaded(MAP_AT, &plain[at..at + WINDOW], Some(&id))
            ),
            "a file without an id must not pass against an image with one"
        );
    }

    /// The real loader snapshot and the real file must agree, end to end: this
    /// is what makes `builds_a_non_empty_table_for_this_process` evidence about
    /// the build-id path rather than only about the byte window.
    ///
    /// Whether a build-id exists at all is the linker's choice, not this code's
    /// (distro toolchains pass `--build-id`; a bare `lld` need not), so an
    /// image without one skips rather than fails.
    #[test]
    fn the_loader_and_the_file_agree_on_this_binary_s_build_id() {
        let exe = std::fs::read("/proc/self/exe").expect("read own image");
        let Some(from_file) = build_id_from_file(&exe) else {
            eprintln!("skipped: this binary was linked without a GNU build-id");
            return;
        };

        let here = the_loader_and_the_file_agree_on_this_binary_s_build_id as *const () as u64;
        let loaded = LoadedImages::snapshot();
        let image = loaded
            .covering(here)
            .expect("the loader must cover this function's own address");
        assert_eq!(
            image.build_id.as_deref(),
            Some(from_file),
            "the loader's in-memory build-id must match the file's"
        );
    }

    #[test]
    fn load_bias_rejects_a_short_program_header_size() {
        for phentsize in [0u16, 8] {
            let mut b = elf(3, &[(PT_LOAD, PF_X, 0, 0, 0x1000, 0x1000)], &[]);
            b[54..56].copy_from_slice(&phentsize.to_le_bytes());
            assert_eq!(
                load_bias(&b, &exec_mapping(), 3),
                None,
                "phentsize {phentsize}"
            );
        }
    }

    /// Many headers pointing at one symbol table must not multiply the work.
    ///
    /// The cap is per module, so it bites only when the sections' combined
    /// entry count exceeds it. This asserts the exact truncation point, with a
    /// control below that collects more.
    #[test]
    fn symbol_reads_are_capped_per_module() {
        // 16 headers over a symbol area of 16 entries: 256 entries uncapped.
        let shdrs = 16usize;
        let slots = 16usize;
        let area = sym_area(0, shdrs);
        let one = (
            SHT_SYMTAB,
            area as u64,
            (slots * SYM_ENTSIZE) as u64,
            SYM_ENTSIZE as u64,
        );
        let mut b = elf(ET_EXEC, &[], &vec![one; shdrs]);
        b.resize(area + slots * SYM_ENTSIZE, 0);
        for i in 0..slots {
            let sym = area + i * SYM_ENTSIZE;
            b[sym + 4] = STT_FUNC;
            b[sym + 8..sym + 16].copy_from_slice(&(SYM_VALUE + i as u64).to_le_bytes());
        }
        let budget = b.len() / SYM_ENTSIZE;
        assert!(
            shdrs * slots > budget,
            "fixture must exceed the budget: {} vs {budget}",
            shdrs * slots
        );

        let mut out = Vec::new();
        collect_func_starts(&b, &exec_mapping(), &mut out);
        assert_eq!(
            out.len(),
            budget,
            "the per-module cap must stop the walk at exactly {budget} reads"
        );

        // Control: one header over the same area stays well under the cap, so
        // the figure above is the cap and not some other limit.
        let mut few = Vec::new();
        let b1 = {
            let area1 = sym_area(0, 1);
            let mut b1 = elf(
                ET_EXEC,
                &[],
                &[(
                    SHT_SYMTAB,
                    area1 as u64,
                    (slots * SYM_ENTSIZE) as u64,
                    SYM_ENTSIZE as u64,
                )],
            );
            b1.resize(area1 + slots * SYM_ENTSIZE, 0);
            for i in 0..slots {
                let sym = area1 + i * SYM_ENTSIZE;
                b1[sym + 4] = STT_FUNC;
                b1[sym + 8..sym + 16].copy_from_slice(&(SYM_VALUE + i as u64).to_le_bytes());
            }
            b1
        };
        collect_func_starts(&b1, &exec_mapping(), &mut few);
        assert_eq!(few.len(), slots, "one header must yield every symbol");
    }

    #[test]
    fn ignores_non_elf_input() {
        let mut out = Vec::new();
        let m = Mapping {
            start: 0x1000,
            offset: 0,
            len: 0x1000,
        };
        collect_func_starts(b"not an elf file at all", &m, &mut out);
        assert!(out.is_empty());
    }

    /// Minimal ELF64 image: header, program headers, section headers.
    /// `phdrs`/`shdrs` are `(type, flags, offset, vaddr_or_unused, filesz, align)`
    /// and `(type, offset, size, entsize)` respectively.
    fn elf(
        e_type: u16,
        phdrs: &[(u32, u32, u64, u64, u64, u64)],
        shdrs: &[(u32, u64, u64, u64)],
    ) -> Vec<u8> {
        let phoff = 64usize;
        let shoff = phoff + phdrs.len() * 56;
        // Symbols live past the headers, so a section pointed at `sym_area`
        // does not overlap the header table and can yield a real entry.
        let sym_area = sym_area(phdrs.len(), shdrs.len());
        let mut b = vec![0u8; sym_area + 4 * SYM_ENTSIZE];
        b[..4].copy_from_slice(ELF_MAGIC);
        b[4] = ELFCLASS64;
        b[16..18].copy_from_slice(&e_type.to_le_bytes());
        b[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        b[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());
        b[58..60].copy_from_slice(&64u16.to_le_bytes());
        b[60..62].copy_from_slice(&(shdrs.len() as u16).to_le_bytes());
        for (i, &(ty, flags, off, vaddr, filesz, align)) in phdrs.iter().enumerate() {
            let p = phoff + i * 56;
            b[p..p + 4].copy_from_slice(&ty.to_le_bytes());
            b[p + 4..p + 8].copy_from_slice(&flags.to_le_bytes());
            b[p + 8..p + 16].copy_from_slice(&off.to_le_bytes());
            b[p + 16..p + 24].copy_from_slice(&vaddr.to_le_bytes());
            b[p + 32..p + 40].copy_from_slice(&filesz.to_le_bytes());
            b[p + 48..p + 56].copy_from_slice(&align.to_le_bytes());
        }
        for (i, &(ty, off, size, entsize)) in shdrs.iter().enumerate() {
            let s = shoff + i * 64;
            b[s + 4..s + 8].copy_from_slice(&ty.to_le_bytes());
            b[s + 24..s + 32].copy_from_slice(&off.to_le_bytes());
            b[s + 32..s + 40].copy_from_slice(&size.to_le_bytes());
            b[s + 56..s + 64].copy_from_slice(&entsize.to_le_bytes());
        }
        // One STT_FUNC symbol with a non-zero st_value in the symbol area.
        b[sym_area + 4] = STT_FUNC;
        b[sym_area + 8..sym_area + 16].copy_from_slice(&SYM_VALUE.to_le_bytes());
        b
    }

    /// Where `elf()` puts its symbol area: past both header tables. Callers
    /// need this to build the section header that points at it, so it is a
    /// function of the header counts rather than a constant.
    fn sym_area(phdrs: usize, shdrs: usize) -> usize {
        64 + phdrs * 56 + shdrs * 64
    }
    const SYM_VALUE: u64 = 0x1234;

    fn exec_mapping() -> Mapping {
        Mapping {
            start: 0,
            offset: 0,
            len: 0x1000,
        }
    }

    /// Control for the reject tests below: well-formed headers must yield the
    /// symbol. Without this, "no symbols" is not evidence that a guard fired.
    #[test]
    fn collects_a_symbol_from_well_formed_headers() {
        let b = elf(
            ET_EXEC,
            &[],
            &[(
                SHT_SYMTAB,
                sym_area(0, 1) as u64,
                SYM_ENTSIZE as u64,
                SYM_ENTSIZE as u64,
            )],
        );
        let mut out = Vec::new();
        collect_func_starts(&b, &exec_mapping(), &mut out);
        assert_eq!(out, vec![SYM_VALUE]);
    }

    #[test]
    fn load_bias_is_zero_for_et_exec() {
        let b = elf(ET_EXEC, &[], &[]);
        let m = Mapping {
            start: 0x40_0000,
            offset: 0,
            len: 0x1000,
        };
        assert_eq!(load_bias(&b, &m, ET_EXEC), Some(0));
    }

    /// With `-z noseparate-code` the executable segment's `p_offset` is not
    /// page-aligned, and the mapping's offset (page-aligned by the kernel)
    /// falls inside the preceding read-only segment. Matching that segment
    /// would shift every symbol by its `p_vaddr - p_offset`.
    #[test]
    fn load_bias_uses_the_executable_segment() {
        const ET_DYN: u16 = 3;
        let b = elf(
            ET_DYN,
            &[
                // R: covers file offsets 0..0x5a0ac at vaddr 0.
                (PT_LOAD, 4, 0x0, 0x0, 0x5a0ac, 0x10000),
                // RX: p_offset 0x5a0b0, vaddr 0x6a0b0 -> vaddr-offset = 0x10000.
                (PT_LOAD, 4 | PF_X, 0x5a0b0, 0x6a0b0, 0x1000, 0x10000),
            ],
            &[],
        );
        // The kernel rounds the executable mapping's offset down to a page.
        let m = Mapping {
            start: 0xaaaa_0006_a000,
            offset: 0x5a000,
            len: 0x1000,
        };
        // runtime = start - p_vaddr - (offset - p_offset)
        let want = 0xaaaa_0006_a000u64
            .wrapping_sub(0x6a0b0)
            .wrapping_sub(0x5a000u64.wrapping_sub(0x5a0b0));
        assert_eq!(load_bias(&b, &m, ET_DYN), Some(want));
    }

    /// A bogus `sh_size` must be rejected rather than driving the symbol loop.
    #[test]
    fn rejects_a_section_larger_than_the_file() {
        let b = elf(
            ET_EXEC,
            &[],
            &[(
                SHT_SYMTAB,
                sym_area(0, 1) as u64,
                u64::MAX,
                SYM_ENTSIZE as u64,
            )],
        );
        let m = Mapping {
            start: 0,
            offset: 0,
            len: 0x1000,
        };
        let mut out = Vec::new();
        collect_func_starts(&b, &m, &mut out);
        assert!(out.is_empty());
    }

    /// A zero `shentsize` would make every iteration re-read one header, with
    /// the inner symbol loop running in full each time.
    #[test]
    fn rejects_a_zero_section_header_size() {
        let mut b = elf(
            ET_EXEC,
            &[],
            &[(
                SHT_SYMTAB,
                sym_area(0, 1) as u64,
                SYM_ENTSIZE as u64,
                SYM_ENTSIZE as u64,
            )],
        );
        b[58..60].copy_from_slice(&0u16.to_le_bytes());
        b[60..62].copy_from_slice(&u16::MAX.to_le_bytes());
        let m = Mapping {
            start: 0,
            offset: 0,
            len: 0x1000,
        };
        let mut out = Vec::new();
        collect_func_starts(&b, &m, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn rejects_a_truncated_section_header_table() {
        let mut b = elf(
            ET_EXEC,
            &[],
            &[(
                SHT_SYMTAB,
                sym_area(0, 1) as u64,
                SYM_ENTSIZE as u64,
                SYM_ENTSIZE as u64,
            )],
        );
        // Claim 4096 section headers that are not in the file.
        b[60..62].copy_from_slice(&4096u16.to_le_bytes());
        let m = Mapping {
            start: 0,
            offset: 0,
            len: 0x1000,
        };
        let mut out = Vec::new();
        collect_func_starts(&b, &m, &mut out);
        assert!(out.is_empty());
    }
}

/// Build cost, reported rather than asserted: it scales with the host binary's
/// symbol count, so any threshold would be flaky. `build()` is called directly
/// rather than through the `OnceLock`, so the figure does not depend on whether
/// another test built the table first.
///
/// ```sh
/// cargo test --release -p dial9-perf-self-profile --lib \
///   func_starts::cost -- --ignored --nocapture
/// ```
#[cfg(test)]
mod cost {
    use super::*;

    #[test]
    #[ignore = "measurement, run explicitly"]
    fn report_build_cost() {
        let modules = exec_mappings();
        let bytes: u64 = modules
            .iter()
            .filter_map(|m| std::fs::metadata(&m.path).ok().map(|md| md.len()))
            .sum();
        let t = std::time::Instant::now();
        let table = build();
        let build_time = t.elapsed();

        let here = report_build_cost as *const () as u64;
        let t = std::time::Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(table.entry_of(std::hint::black_box(here)));
        }
        let per_lookup = t.elapsed() / 1000;

        println!(
            "modules={} mapped={:.1}MiB entries={} build={build_time:?} lookup={per_lookup:?}",
            modules.len(),
            bytes as f64 / 1048576.0,
            table.len()
        );
    }
}

#[cfg(test)]
mod maps_tests {
    use super::*;

    const EXEC: &str =
        "aaaab8f20000-aaaab9116000 r-xp 00020000 103:01 184620971                 /opt/svc/app";

    #[test]
    fn parses_an_executable_file_backed_line() {
        let e = parse_exec_maps_line(EXEC).expect("parsed");
        assert_eq!((e.start, e.end), (0xaaaab8f20000, 0xaaaab9116000));
        assert_eq!(e.offset, 0x20000);
        assert_eq!(e.path, "/opt/svc/app");
        assert_eq!(e.end - e.start, 0x1f6000);
    }

    /// The path is the rest of the line.
    #[test]
    fn keeps_a_path_containing_spaces() {
        let line = EXEC.replace("/opt/svc/app", "/opt/my app/lib.so");
        let e = parse_exec_maps_line(&line).expect("parsed");
        assert_eq!(e.path, "/opt/my app/lib.so");
    }

    /// A replaced file's symbols no longer describe what is mapped.
    #[test]
    fn rejects_a_deleted_mapping() {
        let line = format!("{EXEC} (deleted)");
        assert!(parse_exec_maps_line(&line).is_none());
    }

    #[test]
    fn rejects_non_executable_anonymous_and_pseudo_mappings() {
        for line in [
            // not executable
            &EXEC.replace("r-xp", "r--p"),
            // anonymous
            "aaaab8f20000-aaaab9116000 r-xp 00000000 00:00 0",
            // vDSO and other bracketed names are not file-backed
            "ffffb3f1c000-ffffb3f1e000 r-xp 00000000 00:00 0                          [vdso]",
            "",
        ] {
            assert!(
                parse_exec_maps_line(line).is_none(),
                "should have been skipped: {line:?}"
            );
        }
    }
}

/// One test per row of the rule's decision table.
#[cfg(test)]
mod rule_tests {
    use super::*;

    const LEAF: u64 = 0x40_1000;
    const CALLER: u64 = 0x40_2000;
    const GRANDPARENT: u64 = 0x40_3000;
    const TEXT_END: u64 = 0x40_4000;

    fn table() -> FuncStarts {
        FuncStarts::from_sorted_for_test(vec![LEAF, CALLER, GRANDPARENT], TEXT_END)
    }

    /// Sampled inside a frameless leaf: `fp` is the caller's record, so the
    /// chain reports the grandparent and LR holds the caller. Insert it.
    #[test]
    fn inserts_caller_for_frameless_leaf() {
        assert!(lr_is_missing_caller(
            LEAF + 0x10,              // pc, inside the leaf
            CALLER + 0x20,            // lr, return address into the caller
            Some(GRANDPARENT + 0x30), // *(fp+8), return into the grandparent
            Some(&table()),
        ));
    }

    /// A framed function that already made a call leaves LR pointing back into
    /// itself. The chain is already correct, so inserting would be wrong.
    #[test]
    fn skips_stale_lr_pointing_into_the_sampled_function() {
        assert!(!lr_is_missing_caller(
            CALLER + 0x40,            // pc, inside the framed function
            CALLER + 0x10,            // lr, stale, same function
            Some(GRANDPARENT + 0x30), // *(fp+8), its real caller
            Some(&table()),
        ));
    }

    /// A framed function that has not clobbered LR yet: the saved record and
    /// LR agree, so inserting would duplicate the caller.
    #[test]
    fn skips_when_lr_matches_the_chain() {
        let same = GRANDPARENT + 0x30;
        assert!(!lr_is_missing_caller(
            CALLER + 0x40,
            same,
            Some(same),
            Some(&table())
        ));
    }

    /// Sampled in a prologue before `stp x29, x30` has run: `fp` is still the
    /// caller's record, so this behaves like the frameless case and the frame
    /// would otherwise be lost.
    #[test]
    fn inserts_caller_when_sampled_before_prologue() {
        assert!(lr_is_missing_caller(
            CALLER,             // pc, at the function's first instruction
            GRANDPARENT + 0x10, // lr, return into its caller
            Some(LEAF + 0x8),   // *(fp+8), some older frame
            Some(&table()),
        ));
    }

    /// No table (stripped module, JIT, init not yet run) must leave the chain
    /// unchanged.
    #[test]
    fn without_a_table_nothing_is_inserted() {
        assert!(!lr_is_missing_caller(
            LEAF + 0x10,
            CALLER + 0x20,
            Some(GRANDPARENT + 0x30),
            None
        ));
    }

    /// An address below every known function cannot be attributed, so the
    /// conservative answer is to leave the chain alone.
    #[test]
    fn unresolvable_addresses_are_not_inserted() {
        assert!(!lr_is_missing_caller(
            LEAF + 0x10,
            0x100, // below the text
            Some(GRANDPARENT + 0x30),
            Some(&table()),
        ));
    }

    /// Two addresses in the same unsymbolised span resolve equal, which must
    /// read as "same function" and insert nothing.
    #[test]
    fn same_resolved_function_is_not_inserted() {
        assert!(!lr_is_missing_caller(
            LEAF + 0x10,
            LEAF + 0x40,
            Some(GRANDPARENT + 0x30),
            Some(&table()),
        ));
    }

    /// `x30` reused as a scratch register holds a value that is not a return
    /// address. The text bound rejects it instead of naming the last function.
    #[test]
    fn scratch_lr_outside_the_text_is_not_inserted() {
        for lr in [
            TEXT_END,              // first byte past the text
            0xffff_0000_1234,      // heap-like
            7,                     // small integer
            u64::MAX,              // all ones
            0x00aa_0000_0040_2020, // PAC-signed caller address
        ] {
            assert!(
                !lr_is_missing_caller(LEAF + 0x10, lr, Some(GRANDPARENT + 0x30), Some(&table())),
                "lr {lr:#x} must not produce a frame"
            );
        }
    }
}
