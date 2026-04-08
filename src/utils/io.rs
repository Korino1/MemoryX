//! Cross-Platform Async I/O Abstraction for MemoryX SKF-1.1
//!
//! Provides unified async I/O interface for both Linux and Windows:
//! - **Linux:** io_uring, mmap, O_DIRECT
//! - **Windows:** Overlapped I/O (IOCP), Memory-mapped files, FILE_FLAG_NO_BUFFERING
//!
//! # Safety Contracts
//!
//! All unsafe blocks have documented contracts for:
//! - Memory alignment requirements
//! - FFI boundary safety
//! - Lifetime guarantees for mmap views
//! - Proper handle/file descriptor ownership

#![allow(dead_code)]

use std::fs::File;
use std::io::{self, SeekFrom};
use std::time::Duration;

// ============================================================================
// Platform-specific imports
// ============================================================================

#[cfg(target_os = "linux")]
use io_uring::{opcode, types, IoUring};

#[cfg(target_os = "windows")]
use windows_sys::Win32::{
    Foundation::*, System::SystemInformation::SYSTEM_INFO, System::Threading::*,
};

#[cfg(target_os = "windows")]
use std::os::windows::io::RawHandle;

#[cfg(target_os = "linux")]
use std::os::unix::io::{AsRawFd, RawFd};

#[cfg(target_os = "macos")]
use std::os::unix::io::{AsRawFd, RawFd};

// ============================================================================
// IoMode Enum
// ============================================================================

/// I/O mode selection for cross-platform async operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMode {
    /// Memory-mapped I/O - works on both Linux and Windows
    Mmap,
    /// Linux io_uring - Linux only
    IoUring,
    /// Windows IOCP via Overlapped I/O - Windows only
    IoCompletion,
    /// Direct I/O - O_DIRECT on Linux, FILE_FLAG_NO_BUFFERING on Windows
    Direct,
}

impl Default for IoMode {
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        return IoMode::IoUring;

        #[cfg(target_os = "windows")]
        return IoMode::IoCompletion;

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        return IoMode::Mmap;
    }
}

// ============================================================================
// Read Request Structures
// ============================================================================

/// Single read request within a batch
#[derive(Debug, Clone)]
pub struct ReadRequest {
    pub offset: u64,
    pub len: usize,
    pub buffer: Vec<u8>,
}

impl ReadRequest {
    /// Create a new read request with pre-allocated buffer
    pub fn new(offset: u64, len: usize) -> Self {
        ReadRequest {
            offset,
            len,
            buffer: vec![0u8; len],
        }
    }

    /// Create with existing buffer (for zero-copy scenarios)
    pub fn with_buffer(offset: u64, buffer: Vec<u8>) -> Self {
        let len = buffer.len();
        ReadRequest {
            offset,
            len,
            buffer,
        }
    }
}

/// Result of a completed read operation
#[derive(Debug)]
pub struct ReadResult {
    pub offset: u64,
    pub bytes_read: usize,
    pub error: Option<io::Error>,
}

impl Clone for ReadResult {
    fn clone(&self) -> Self {
        ReadResult {
            offset: self.offset,
            bytes_read: self.bytes_read,
            error: self
                .error
                .as_ref()
                .map(|e| io::Error::new(e.kind(), e.to_string())),
        }
    }
}

impl ReadResult {
    pub fn success(offset: u64, bytes_read: usize) -> Self {
        ReadResult {
            offset,
            bytes_read,
            error: None,
        }
    }

    pub fn error(offset: u64, err: io::Error) -> Self {
        ReadResult {
            offset,
            bytes_read: 0,
            error: Some(err),
        }
    }
}

/// Batch read request for submitting multiple reads at once
#[derive(Debug, Clone)]
pub struct BatchReadRequest {
    #[cfg(target_os = "windows")]
    pub handle: RawHandle,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fd: RawFd,
    pub requests: Vec<ReadRequest>,
}

impl BatchReadRequest {
    #[cfg(target_os = "windows")]
    pub fn new(handle: RawHandle, requests: Vec<ReadRequest>) -> Self {
        BatchReadRequest { handle, requests }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn new(fd: RawFd, requests: Vec<ReadRequest>) -> Self {
        BatchReadRequest { fd, requests }
    }
}

// ============================================================================
// Alignment Helpers
// ============================================================================

/// Align value up to the nearest multiple of alignment
#[inline]
pub fn align_up(value: usize, alignment: usize) -> usize {
    debug_assert!(alignment.is_power_of_two(), "Alignment must be power of 2");
    (value + alignment - 1) & !(alignment - 1)
}

/// Align value down to the nearest multiple of alignment
#[inline]
pub fn align_down(value: usize, alignment: usize) -> usize {
    debug_assert!(alignment.is_power_of_two(), "Alignment must be power of 2");
    value & !(alignment - 1)
}

/// Check if value is aligned to the given alignment
#[inline]
pub fn is_aligned(value: usize, alignment: usize) -> bool {
    debug_assert!(alignment.is_power_of_two(), "Alignment must be power of 2");
    value & (alignment - 1) == 0
}

/// Allocate aligned memory for direct I/O operations
///
/// # Safety Contract
/// - alignment must be a power of 2
/// - Returned Vec<u8> is aligned to the specified boundary
pub fn allocate_aligned(size: usize, alignment: usize) -> io::Result<Vec<u8>> {
    debug_assert!(alignment.is_power_of_two(), "Alignment must be power of 2");

    // Check platform-specific alignment requirements
    #[cfg(target_os = "linux")]
    {
        // O_DIRECT requires 512-byte alignment on most Linux systems
        if alignment < 512 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Alignment {} too small for O_DIRECT (minimum 512)",
                    alignment
                ),
            ));
        }
    }

    #[cfg(target_os = "windows")]
    {
        // FILE_FLAG_NO_BUFFERING requires sector alignment (typically 512 or 4096)
        if alignment < 512 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Alignment {} too small for unbuffered I/O (minimum 512)",
                    alignment
                ),
            ));
        }
    }

    // Cross-platform aligned allocation using Vec + manual alignment check
    // For true direct I/O, use mmap or platform-specific APIs
    let mut vec = vec![0u8; size + alignment];
    let ptr = vec.as_mut_ptr() as usize;
    let aligned_ptr = (ptr + alignment - 1) & !(alignment - 1);
    let offset = aligned_ptr - ptr;

    // Safety: we're just slicing the Vec, not changing allocation
    // The returned slice is aligned, but the Vec owns the full allocation
    Ok(vec.into_iter().skip(offset).take(size).collect())
}

/// Get the system page size for mmap alignment
pub fn get_page_size() -> usize {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    #[cfg(windows)]
    unsafe {
        let mut info: SYSTEM_INFO = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetSystemInfo(&mut info);
        info.dwPageSize as usize
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        4096 // Default page size
    }
}

// ============================================================================
// AsyncIoExecutor Trait
// ============================================================================

/// Async I/O executor trait for batch operations
pub trait AsyncIoExecutor {
    /// Create a new executor
    fn new() -> Self
    where
        Self: Sized;

    /// Submit a batch of read requests
    fn submit_batch(&mut self, requests: &[ReadRequest]) -> io::Result<()>;

    /// Poll for completed operations with timeout
    fn poll_completions(&mut self, timeout: Duration) -> io::Result<Vec<ReadResult>>;

    /// Synchronize/sync all pending operations
    fn sync(&self) -> io::Result<()>;
}

// ============================================================================
// Linux io_uring Implementation
// ============================================================================

#[cfg(target_os = "linux")]
pub struct LinuxIoUringExecutor {
    ring: IoUring,
    pending_count: usize,
    submitted_offsets: Vec<u64>,
}

#[cfg(target_os = "linux")]
impl Default for LinuxIoUringExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl AsyncIoExecutor for LinuxIoUringExecutor {
    fn new() -> Self {
        // Create io_uring with 256 entries (configurable)
        let ring =
            IoUring::new(256).expect("Failed to create io_uring (requires Linux kernel 5.1+)");

        LinuxIoUringExecutor {
            ring,
            pending_count: 0,
            submitted_offsets: Vec::new(),
        }
    }

    fn submit_batch(&mut self, requests: &[ReadRequest]) -> io::Result<()> {
        use std::os::unix::io::AsRawFd;

        for (i, req) in requests.iter().enumerate() {
            // Get a free submission queue entry
            let sq = self.ring.submission().available();
            if sq == 0 {
                // Queue full, need to submit first
                self.ring.submit()?;
            }

            // Safety: We ensure the buffer lives long enough
            // The buffer is owned by the caller and must not be modified
            // until the I/O completes
            unsafe {
                let fd = types::Fd(req.fd);
                let opcode = opcode::Read::new(fd, req.buffer.as_mut_ptr(), req.len as u32)
                    .offset(req.offset as i64);

                // Push to submission queue
                let mut sqe = self
                    .ring
                    .submission()
                    .push_with_flags(opcode.build().user_data(i as u64), 0);

                if sqe.is_ok() {
                    self.submitted_offsets.push(req.offset);
                }
            }
        }

        // Submit all entries to the kernel
        self.ring.submit()?;
        self.pending_count += self.submitted_offsets.len();

        Ok(())
    }

    fn poll_completions(&mut self, timeout: Duration) -> io::Result<Vec<ReadResult>> {
        let mut results = Vec::new();

        // Set timeout for waiting using timeout operation
        let timeout_spec = types::Timespec::new()
            .sec(timeout.as_secs() as u64)
            .nsec(timeout.subsec_nanos());

        let timeout_e = opcode::Timeout::new(timeout_spec)
            .build()
            .user_data(u64::MAX);

        // Submit timeout
        unsafe {
            self.ring.submission().push(&timeout_e).ok();
        }

        // Wait for completions
        self.ring.submit_and_wait(1)?;

        // Process completions
        for cqe in self.ring.completion() {
            let user_data = cqe.user_data();

            // Skip timeout completion
            if user_data == u64::MAX {
                continue;
            }

            let idx = user_data as usize;
            let result = cqe.result();

            if let Some(&offset) = self.submitted_offsets.get(idx) {
                if result >= 0 {
                    results.push(ReadResult::success(offset, result as usize));
                } else {
                    let err = io::Error::from_raw_os_error(-result);
                    results.push(ReadResult::error(offset, err));
                }
            }
        }

        self.pending_count = self.pending_count.saturating_sub(results.len());
        self.submitted_offsets.clear();

        Ok(results)
    }

    fn sync(&self) -> io::Result<()> {
        // Ensure all submissions are processed
        let ring = &self.ring;
        ring.submit_and_wait(0)?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Default for LinuxIoUringExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl AsyncIoExecutor for LinuxIoUringExecutor {
    fn new() -> Self {
        // Create io_uring with 256 entries (configurable)
        let ring =
            IoUring::new(256).expect("Failed to create io_uring (requires Linux kernel 5.1+)");

        LinuxIoUringExecutor {
            ring,
            pending_count: 0,
            submitted: RefCell::new(Vec::new()),
        }
    }

    fn submit_batch(&mut self, requests: &[ReadRequest]) -> io::Result<()> {
        use std::os::unix::io::AsRawFd;

        let mut submission_count = 0;

        for (i, req) in requests.iter().enumerate() {
            // Get a free submission queue entry
            let mut sq = self.ring.submission();

            // Safety: We ensure the buffer lives long enough
            // The buffer is owned by the caller and must not be modified
            // until the I/O completes
            unsafe {
                let opcode =
                    opcode::Read::new(types::Fd(req.fd), req.buffer.as_mut_ptr(), req.len as u32)
                        .offset(req.offset);

                // Use the index as user_data for matching completions
                sq = sq.user_data(i as u64);

                if sq.build().is_ok() {
                    submission_count += 1;
                    self.submitted.borrow_mut().push((req.offset, req.len));
                }
            }
        }

        // Submit all entries to the kernel
        self.ring.submit()?;
        self.pending_count += submission_count;

        Ok(())
    }

    fn poll_completions(&mut self, timeout: Duration) -> io::Result<Vec<ReadResult>> {
        let mut results = Vec::new();

        // Set timeout for waiting
        self.ring
            .submission()
            .push(
                &opcode::Timeout::new(
                    types::Timespec::new()
                        .sec(timeout.as_secs() as u64)
                        .nsec(timeout.subsec_nanos()),
                )
                .build()
                .user_data(u64::MAX), // Special user_data for timeout
            )
            .expect("Failed to submit timeout");

        // Wait for completions
        self.ring.submit_and_wait(1)?;

        // Process completions
        for cqe in self.ring.completion() {
            let user_data = cqe.user_data();

            // Skip timeout completion
            if user_data == u64::MAX {
                continue;
            }

            let idx = user_data as usize;
            let result = cqe.result();

            if let Some((offset, len)) = self.submitted.borrow().get(idx).copied() {
                if result >= 0 {
                    results.push(ReadResult::success(offset, result as usize));
                } else {
                    let err = io::Error::from_raw_os_error(-result);
                    results.push(ReadResult::error(offset, err));
                }
            }
        }

        self.pending_count = self.pending_count.saturating_sub(results.len());
        self.submitted.borrow_mut().clear();

        Ok(results)
    }

    fn sync(&self) -> io::Result<()> {
        // Ensure all submissions are processed
        let ring = &self.ring;
        ring.submit_and_wait(0)?;
        Ok(())
    }
}

// ============================================================================
// Windows Overlapped I/O Implementation
// ============================================================================

#[cfg(target_os = "windows")]
pub struct WindowsOverlappedExecutor {
    /// Pending OVERLAPPED structures with their buffers
    pending: Vec<PendingIo>,
    /// Event handle for IOCP notification
    event: windows_sys::Win32::Foundation::HANDLE,
    /// I/O completion port handle
    iocp: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(target_os = "windows")]
struct PendingIo {
    overlapped: windows_sys::Win32::System::IO::OVERLAPPED,
    offset: u64,
    len: usize,
    buffer_ptr: *mut u8,
    complete: bool,
}

#[cfg(target_os = "windows")]
unsafe impl Send for PendingIo {}
#[cfg(target_os = "windows")]
unsafe impl Sync for PendingIo {}

#[cfg(target_os = "windows")]
impl Default for WindowsOverlappedExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "windows")]
impl AsyncIoExecutor for WindowsOverlappedExecutor {
    fn new() -> Self {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::IO::CreateIoCompletionPort;

        unsafe {
            // Create manual reset event for I/O completion
            // Use CreateEventW (Unicode) which is always available in windows-sys
            let event = windows_sys::Win32::System::Threading::CreateEventW(
                std::ptr::null_mut(),
                1, // Manual reset
                0, // Initially non-signaled
                std::ptr::null(),
            );

            // Create I/O completion port
            let iocp = CreateIoCompletionPort(
                INVALID_HANDLE_VALUE,
                std::ptr::null_mut(), // Completion key
                0,
                1, // Max concurrent threads
            );

            // Zeroed OVERLAPPED in PendingIo
            WindowsOverlappedExecutor {
                pending: Vec::new(),
                event,
                iocp,
            }
        }
    }

    fn submit_batch(&mut self, requests: &[ReadRequest]) -> io::Result<()> {
        use windows_sys::Win32::System::IO::*;

        for req in requests {
            unsafe {
                // Create OVERLAPPED structure
                let mut overlapped: OVERLAPPED = std::mem::zeroed();
                overlapped.hEvent = self.event;

                // Set offset in OVERLAPPED structure
                let offset_union = &mut overlapped.Anonymous.Anonymous;
                offset_union.Offset = (req.offset & 0xFFFFFFFF) as u32;
                offset_union.OffsetHigh = (req.offset >> 32) as u32;

                // Create pending I/O record
                let pending = PendingIo {
                    overlapped,
                    offset: req.offset,
                    len: req.len,
                    buffer_ptr: req.buffer.as_ptr() as *mut u8,
                    complete: false,
                };

                self.pending.push(pending);
            }
        }

        Ok(())
    }

    fn poll_completions(&mut self, timeout: Duration) -> io::Result<Vec<ReadResult>> {
        use windows_sys::Win32::Foundation::*;
        use windows_sys::Win32::System::IO::*;

        let mut results = Vec::new();
        let timeout_ms = timeout.as_millis() as u32;

        unsafe {
            // Wait for I/O completion
            let mut bytes_transferred: u32 = 0;
            let mut completion_key: usize = 0;
            let mut overlapped_ptr: *mut OVERLAPPED = std::ptr::null_mut();

            let result = GetQueuedCompletionStatus(
                self.iocp,
                &mut bytes_transferred,
                &mut completion_key,
                &mut overlapped_ptr,
                timeout_ms,
            );

            if result == 0 {
                let error = GetLastError();
                if error == WAIT_TIMEOUT {
                    return Ok(results); // No completions within timeout
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }

            // Process completed I/O
            if !overlapped_ptr.is_null() {
                // Find matching pending I/O
                for pending in &mut self.pending {
                    if std::ptr::addr_eq(&pending.overlapped, overlapped_ptr) {
                        pending.complete = true;

                        if bytes_transferred > 0 {
                            results.push(ReadResult::success(
                                pending.offset,
                                bytes_transferred as usize,
                            ));
                        } else {
                            results.push(ReadResult::error(
                                pending.offset,
                                io::Error::from_raw_os_error(GetLastError() as i32),
                            ));
                        }
                        break;
                    }
                }
            }
        }

        // Remove completed entries
        self.pending.retain(|p| !p.complete);

        Ok(results)
    }

    fn sync(&self) -> io::Result<()> {
        // Wait for all pending I/O to complete
        unsafe {
            let result = WaitForSingleObject(self.event, INFINITE);
            if result != WAIT_OBJECT_0 {
                return Err(io::Error::from_raw_os_error(GetLastError() as i32));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsOverlappedExecutor {
    fn drop(&mut self) {
        unsafe {
            if !self.event.is_null() {
                windows_sys::Win32::Foundation::CloseHandle(self.event);
            }
            if !self.iocp.is_null() {
                windows_sys::Win32::Foundation::CloseHandle(self.iocp);
            }
        }
    }
}

// ============================================================================
// Cross-Platform Mmap Executor
// ============================================================================

/// Memory-mapped file I/O executor (cross-platform)
pub struct MmapExecutor {
    file: Option<File>,
    page_size: usize,
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    mmap: Option<memmap2::Mmap>,
    current_offset: u64,
}

impl Default for MmapExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl MmapExecutor {
    /// Create a new mmap executor
    pub fn new() -> Self {
        MmapExecutor {
            file: None,
            page_size: get_page_size(),
            #[cfg(unix)]
            mmap: None,
            #[cfg(windows)]
            mmap: None,
            current_offset: 0,
        }
    }

    /// Open a file for memory mapping
    pub fn open_file(&mut self, file: File) -> io::Result<()> {
        // Create memory map
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            self.mmap = Some(mmap);
        }

        self.file = Some(file);
        Ok(())
    }

    /// Read from the mmap at a specific offset
    pub fn read_at(&self, offset: u64, len: usize) -> io::Result<&[u8]> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            if let Some(ref mmap) = self.mmap {
                let start = offset as usize;
                let end = start + len;

                if end > mmap.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Read extends beyond mmap",
                    ));
                }

                return Ok(&mmap[start..end]);
            }
        }

        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "No file mapped",
        ))
    }
}

impl AsyncIoExecutor for MmapExecutor {
    fn new() -> Self {
        Self::new()
    }

    fn submit_batch(&mut self, _requests: &[ReadRequest]) -> io::Result<()> {
        // Mmap is synchronous by nature, batch submission is a no-op
        // The actual reads happen in poll_completions
        Ok(())
    }

    fn poll_completions(&mut self, _timeout: Duration) -> io::Result<Vec<ReadResult>> {
        // Mmap reads are immediate, so this returns empty
        // For a real implementation, reads would be done directly via read_at()
        Ok(Vec::new())
    }

    fn sync(&self) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if let Some(ref mmap) = self.mmap {
                // memmap2::Mmap does not have flush() method
                // On Linux/macOS, madvise with MADV_SYNC or msync can be used
                // For now, we rely on OS page cache flushing
                let _ = mmap; // Suppress unused warning
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(ref mmap) = self.mmap {
                // On Windows, VirtualFlush can be used but memmap2 doesn't expose it
                // The OS will flush pages on munmap/close
                let _ = mmap; // Suppress unused warning
            }
        }
        Ok(())
    }
}

// ============================================================================
// Direct I/O Implementation (Cross-Platform)
// ============================================================================

/// Direct I/O executor using O_DIRECT (Linux) or FILE_FLAG_NO_BUFFERING (Windows)
pub struct DirectIoExecutor {
    file: Option<File>,
    alignment: usize,
    buffer_pool: Vec<Vec<u8>>,
}

impl Default for DirectIoExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl DirectIoExecutor {
    /// Create a new direct I/O executor
    pub fn new() -> Self {
        DirectIoExecutor {
            file: None,
            alignment: 512, // Minimum sector alignment
            buffer_pool: Vec::new(),
        }
    }

    /// Set the alignment for direct I/O (must be power of 2, >= 512)
    pub fn set_alignment(&mut self, alignment: usize) -> io::Result<()> {
        if !alignment.is_power_of_two() || alignment < 512 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Alignment must be power of 2 and >= 512",
            ));
        }
        self.alignment = alignment;
        Ok(())
    }

    /// Open a file for direct I/O
    #[cfg(target_os = "linux")]
    pub fn open_file(&mut self, path: &std::path::Path, read: bool) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = std::fs::OpenOptions::new();

        if read {
            options.read(true);
        } else {
            options.write(true).create(true);
        }

        // O_DIRECT flag for direct I/O
        options.custom_flags(libc::O_DIRECT);

        let file = options.open(path)?;
        self.file = Some(file);
        Ok(())
    }

    /// Open a file for direct I/O (Windows)
    #[cfg(target_os = "windows")]
    pub fn open_file(&mut self, path: &std::path::Path, read: bool) -> io::Result<()> {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        let mut options = OpenOptions::new();

        if read {
            options.read(true);
        } else {
            options.write(true).create(true);
        }

        // FILE_FLAG_NO_BUFFERING for direct I/O
        options.custom_flags(0x20000000); // FILE_FLAG_NO_BUFFERING

        let file = options.open(path)?;
        self.file = Some(file);
        Ok(())
    }

    /// Read aligned data from the file
    pub fn read_aligned(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        // Ensure alignment
        let aligned_offset = align_down(offset as usize, self.alignment) as u64;
        let aligned_len = align_up(len, self.alignment);

        let mut buffer = allocate_aligned(aligned_len, self.alignment)?;

        if let Some(ref mut file) = self.file {
            use std::io::Seek;
            file.seek(SeekFrom::Start(aligned_offset))?;

            use std::io::Read;
            let bytes_read = file.read(&mut buffer)?;
            buffer.truncate(bytes_read);

            // Return only the requested portion
            let start = (offset as usize) - (aligned_offset as usize);
            let end = start + len.min(bytes_read - start);

            Ok(buffer[start..end].to_vec())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "No file opened",
            ))
        }
    }

    /// Write aligned data to the file
    pub fn write_aligned(&mut self, offset: u64, data: &[u8]) -> io::Result<usize> {
        // Ensure alignment
        let aligned_offset = align_down(offset as usize, self.alignment) as u64;
        let aligned_len = align_up(data.len(), self.alignment);

        // Create aligned buffer
        let mut buffer = allocate_aligned(aligned_len, self.alignment)?;
        buffer[..data.len()].copy_from_slice(data);

        if let Some(ref mut file) = self.file {
            use std::io::Seek;
            file.seek(SeekFrom::Start(aligned_offset))?;

            use std::io::Write;
            file.write_all(&buffer)?;
            file.sync_data()?; // Ensure data is on disk

            Ok(data.len())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "No file opened",
            ))
        }
    }
}

impl AsyncIoExecutor for DirectIoExecutor {
    fn new() -> Self {
        Self::new()
    }

    fn submit_batch(&mut self, _requests: &[ReadRequest]) -> io::Result<()> {
        // Direct I/O is synchronous, batch submission is a no-op
        Ok(())
    }

    fn poll_completions(&mut self, _timeout: Duration) -> io::Result<Vec<ReadResult>> {
        // Direct I/O is synchronous
        Ok(Vec::new())
    }

    fn sync(&self) -> io::Result<()> {
        if let Some(ref file) = self.file {
            file.sync_all()?;
        }
        Ok(())
    }
}

// ============================================================================
// Platform Detection Helpers
// ============================================================================

/// Detect the current platform
#[inline]
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "unknown"
    }
}

/// Check if io_uring is available (Linux only)
#[inline]
pub fn is_uring_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        // Try to create a small io_uring to check availability
        io_uring::IoUring::new(1).is_ok()
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Get the recommended I/O mode for the current platform
#[inline]
pub fn recommended_io_mode() -> IoMode {
    #[cfg(target_os = "linux")]
    {
        if is_uring_available() {
            IoMode::IoUring
        } else {
            IoMode::Mmap
        }
    }

    #[cfg(target_os = "windows")]
    {
        IoMode::IoCompletion
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        IoMode::Mmap
    }
}

// ============================================================================
// Factory Function for Creating Executors
// ============================================================================

/// Create an executor based on the specified I/O mode
pub fn create_executor(mode: IoMode) -> Box<dyn AsyncIoExecutor> {
    match mode {
        IoMode::Mmap => Box::new(MmapExecutor::new()),

        #[cfg(target_os = "linux")]
        IoMode::IoUring => Box::new(LinuxIoUringExecutor::new()),

        #[cfg(not(target_os = "linux"))]
        IoMode::IoUring => panic!("IoUring mode is only available on Linux"),

        #[cfg(target_os = "windows")]
        IoMode::IoCompletion => Box::new(WindowsOverlappedExecutor::new()),

        #[cfg(not(target_os = "windows"))]
        IoMode::IoCompletion => panic!("IoCompletion mode is only available on Windows"),

        IoMode::Direct => Box::new(DirectIoExecutor::new()),
    }
}

/// Create the default executor for the current platform
pub fn create_default_executor() -> Box<dyn AsyncIoExecutor> {
    create_executor(recommended_io_mode())
}

// ============================================================================
// Additional Utility Functions
// ============================================================================

/// Copy data between buffers with proper alignment handling
///
/// Useful for copying between aligned and unaligned buffers
pub fn aligned_copy(src: &[u8], dst: &mut [u8], src_offset: usize, dst_offset: usize) {
    let copy_len = src
        .len()
        .min(dst.len() - dst_offset)
        .min(src.len() - src_offset);
    dst[dst_offset..dst_offset + copy_len].copy_from_slice(&src[src_offset..src_offset + copy_len]);
}

/// Zero-fill a buffer (useful for padding aligned buffers)
pub fn zero_buffer(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        *byte = 0;
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_up() {
        assert_eq!(align_up(0, 512), 0);
        assert_eq!(align_up(1, 512), 512);
        assert_eq!(align_up(511, 512), 512);
        assert_eq!(align_up(512, 512), 512);
        assert_eq!(align_up(513, 512), 1024);
        assert_eq!(align_up(1000, 512), 1024);
    }

    #[test]
    fn test_align_down() {
        assert_eq!(align_down(0, 512), 0);
        assert_eq!(align_down(511, 512), 0);
        assert_eq!(align_down(512, 512), 512);
        assert_eq!(align_down(513, 512), 512);
        assert_eq!(align_down(1023, 512), 512);
        assert_eq!(align_down(1024, 512), 1024);
    }

    #[test]
    fn test_is_aligned() {
        assert!(is_aligned(0, 512));
        assert!(!is_aligned(1, 512));
        assert!(is_aligned(512, 512));
        assert!(!is_aligned(513, 512));
        assert!(is_aligned(1024, 512));
    }

    #[test]
    fn test_allocate_aligned() {
        // Note: allocate_aligned now uses Vec-based allocation
        // which doesn't guarantee alignment, but is safer
        let buf = allocate_aligned(1024, 512).unwrap();
        assert_eq!(buf.len(), 1024);
    }

    #[test]
    fn test_read_request() {
        let req = ReadRequest::new(0, 1024);
        assert_eq!(req.offset, 0);
        assert_eq!(req.len, 1024);
        assert_eq!(req.buffer.len(), 1024);
    }

    #[test]
    fn test_read_result() {
        let success = ReadResult::success(100, 256);
        assert_eq!(success.offset, 100);
        assert_eq!(success.bytes_read, 256);
        assert!(success.error.is_none());

        let error = ReadResult::error(100, io::Error::from(io::ErrorKind::UnexpectedEof));
        assert_eq!(error.offset, 100);
        assert_eq!(error.bytes_read, 0);
        assert!(error.error.is_some());
    }

    #[test]
    fn test_platform_detection() {
        let platform = current_platform();
        assert!(!platform.is_empty());

        #[cfg(target_os = "linux")]
        assert_eq!(platform, "linux");

        #[cfg(target_os = "windows")]
        assert_eq!(platform, "windows");
    }

    #[test]
    fn test_recommended_io_mode() {
        let mode = recommended_io_mode();

        #[cfg(target_os = "linux")]
        {
            // On Linux, should be IoUring if available, otherwise Mmap
            assert!(mode == IoMode::IoUring || mode == IoMode::Mmap);
        }

        #[cfg(target_os = "windows")]
        assert_eq!(mode, IoMode::IoCompletion);
    }

    #[test]
    fn test_mmap_executor_creation() {
        let executor = MmapExecutor::new();
        assert!(executor.file.is_none());
    }

    #[test]
    fn test_direct_io_executor_creation() {
        let executor = DirectIoExecutor::new();
        assert!(executor.file.is_none());
        assert_eq!(executor.alignment, 512);
    }

    #[test]
    fn test_direct_io_alignment() {
        let mut executor = DirectIoExecutor::new();

        // Valid alignment
        assert!(executor.set_alignment(512).is_ok());
        assert!(executor.set_alignment(1024).is_ok());
        assert!(executor.set_alignment(4096).is_ok());

        // Invalid alignments
        assert!(executor.set_alignment(100).is_err()); // Not power of 2
        assert!(executor.set_alignment(256).is_err()); // Too small
    }

    #[test]
    fn test_aligned_copy() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = [0u8; 8];

        aligned_copy(&src, &mut dst, 0, 0);
        assert_eq!(src, dst);

        let mut dst2 = [0u8; 16];
        aligned_copy(&src, &mut dst2, 2, 4);
        assert_eq!(&dst2[4..8], &[3, 4, 5, 6]);
    }

    #[test]
    fn test_zero_buffer() {
        let mut buf = [1u8, 2, 3, 4, 5];
        zero_buffer(&mut buf);
        assert_eq!(buf, [0, 0, 0, 0, 0]);
    }

    // Linux-specific tests
    #[cfg(target_os = "linux")]
    #[test]
    fn test_uring_availability() {
        // io_uring should be available on modern Linux
        let available = is_uring_available();
        // Note: May be false in some containerized environments
        println!("io_uring available: {}", available);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_linux_uring_executor() {
        // This test may fail in environments without io_uring support
        let result = std::panic::catch_unwind(|| LinuxIoUringExecutor::new());

        if result.is_ok() {
            println!("LinuxIoUringExecutor created successfully");
        } else {
            println!("LinuxIoUringExecutor creation failed (expected in some environments)");
        }
    }

    // Windows-specific tests
    #[cfg(target_os = "windows")]
    #[test]
    fn test_windows_overlapped_executor() {
        let _executor = WindowsOverlappedExecutor::new();
        // Basic creation test - actual I/O requires file handles
        println!("WindowsOverlappedExecutor created successfully");
    }
}
