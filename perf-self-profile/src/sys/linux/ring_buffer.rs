//! Lock-free ring buffer consumer for the perf mmap'd region.
//!
//! The kernel writes records into a circular buffer. We read `data_head` (volatile),
//! parse records, then advance `data_tail` to tell the kernel we're done.

use std::mem;
use std::ptr;
use std::sync::atomic::{Ordering, fence};

use perf_event_open_sys::bindings::{perf_event_header, perf_event_mmap_page};

/// A mapped perf ring buffer.
pub(crate) struct RingBuffer {
    /// Pointer to the mmap'd region (metadata page + data pages).
    base: *mut u8,
    /// Size of the data region only (excluding the metadata page).
    data_size: u64,
    /// Total mmap size (metadata page + data pages), for munmap.
    mmap_size: usize,
    /// Our current read position.
    position: u64,
}

// SAFETY:
// `RingBuffer` owns the mapping and is not `Sync`, so only the owning thread can
// consume it. Moving that ownership to another thread does not change the
// mapping's validity. Cursor access follows perf's Acquire/Release protocol.
unsafe impl Send for RingBuffer {}

impl RingBuffer {
    /// Create a new RingBuffer from an already-mmap'd pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    ///
    /// - `base` is non-null, page-aligned, and points to a live writable
    ///   perf-event mapping of at least `mmap_size` bytes.
    /// - The mapping starts with a `perf_event_mmap_page`, followed by
    ///   `data_size` bytes of ring data.
    /// - `data_size` is non-zero, fits in `usize`, and
    ///   `page_size() + data_size <= mmap_size`.
    /// - No other owner unmaps the first `mmap_size` bytes. Ownership of that
    ///   range transfers to the returned `RingBuffer`, which unmaps it in
    ///   `Drop`.
    pub unsafe fn new(base: *mut u8, data_size: u64, mmap_size: usize) -> Self {
        RingBuffer {
            base,
            data_size,
            mmap_size,
            position: 0,
        }
    }

    /// Returns true if there are unread records in the buffer.
    pub fn has_data(&self) -> bool {
        let head = self.read_head();
        head != self.position
    }

    /// Iterate over all pending records. Each record is provided as a `RawRecord`.
    /// After the callback returns for each record, the ring buffer tail is advanced.
    pub fn for_each_record<F>(&mut self, mut f: F)
    where
        F: FnMut(RawRecord<'_>),
    {
        loop {
            let head = self.read_head();
            if head == self.position {
                break;
            }

            let data = self.data_slice();
            let pos = (self.position % self.data_size) as usize;

            let header: perf_event_header = if pos + mem::size_of::<perf_event_header>()
                <= data.len()
            {
                unsafe { ptr::read_unaligned(data.as_ptr().add(pos) as *const perf_event_header) }
            } else {
                let mut buf = [0u8; mem::size_of::<perf_event_header>()];
                for (i, b) in buf.iter_mut().enumerate() {
                    *b = data[(pos + i) % data.len()];
                }
                unsafe { ptr::read_unaligned(buf.as_ptr() as *const perf_event_header) }
            };

            let record_size = header.size as usize;
            let body_offset = mem::size_of::<perf_event_header>();
            let body_size = record_size - body_offset;
            let body_start = (pos + body_offset) % data.len();

            let record = if body_start + body_size <= data.len() {
                RawRecord {
                    header,
                    body: RecordBody::Contiguous(&data[body_start..body_start + body_size]),
                }
            } else {
                RawRecord {
                    header,
                    body: RecordBody::Split(
                        &data[body_start..],
                        &data[..body_size - (data.len() - body_start)],
                    ),
                }
            };

            f(record);

            self.position += record_size as u64;
            self.write_tail(self.position);
        }
    }

    fn read_head(&self) -> u64 {
        let page = self.base.cast::<perf_event_mmap_page>();
        // SAFETY:
        // `RingBuffer::new` requires `base` to point to a live, aligned
        // perf-event metadata page. `addr_of!` creates no Rust reference to the
        // kernel-mutated mapping. The volatile load reads the kernel-published
        // cursor, and the Acquire fence orders subsequent data reads after it.
        unsafe {
            let head = ptr::read_volatile(ptr::addr_of!((*page).data_head));
            fence(Ordering::Acquire);
            head
        }
    }

    fn write_tail(&self, value: u64) {
        let page = self.base.cast::<perf_event_mmap_page>();
        // SAFETY:
        // The mapping invariant from `RingBuffer::new` keeps the metadata page
        // live and writable. `addr_of_mut!` creates no exclusive Rust reference
        // to memory also accessed by the kernel. The Release fence publishes all
        // preceding record reads before the volatile tail update.
        unsafe {
            fence(Ordering::Release);
            ptr::write_volatile(ptr::addr_of_mut!((*page).data_tail), value);
        }
    }

    fn data_slice(&self) -> &[u8] {
        unsafe {
            let data_ptr = self.base.add(page_size()); // skip metadata page
            std::slice::from_raw_parts(data_ptr, self.data_size as usize)
        }
    }
}

pub(crate) fn page_size() -> usize {
    // SAFETY:
    // `sysconf` does not access caller-provided memory, and `_SC_PAGESIZE`
    // requests a process-wide constant.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(size > 0, "sysconf(_SC_PAGESIZE) failed");
    size as usize
}

impl Drop for RingBuffer {
    fn drop(&mut self) {
        // SAFETY:
        // `RingBuffer::new` transfers ownership of the first `mmap_size` bytes
        // of a live mapping. This is the only unmap path for that range.
        unsafe {
            libc::munmap(self.base.cast::<libc::c_void>(), self.mmap_size);
        };
    }
}

/// A raw record read from the ring buffer, before parsing.
pub struct RawRecord<'a> {
    pub header: perf_event_header,
    pub body: RecordBody<'a>,
}

/// The body of a record, which may be contiguous or split across the ring buffer wrap point.
pub enum RecordBody<'a> {
    Contiguous(&'a [u8]),
    Split(&'a [u8], &'a [u8]),
}
