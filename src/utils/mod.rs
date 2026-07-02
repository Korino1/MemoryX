//! Utility functions for MemoryX SKF-1.1 implementation.
//!
//! Provides:
//! - HLC (Hybrid Logical Clock) implementation
//! - Varint encoding/decoding (zigzag for i64)
//! - Bit packing utilities for blocks of 128 elements
//! - CRC32 calculation wrappers
//! - Cross-platform async I/O abstraction (io module)
//! - Runtime CPU capability detection for portable accelerated builds

#![allow(dead_code)]

pub mod cpu;
pub mod io;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// CRC32 lookup table (precomputed)
static CRC32_TABLE: [u32; 256] = compute_crc32_table();

const fn compute_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB88320
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
}

/// Calculate CRC32 checksum for a byte slice
#[inline]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ byte as u32) & 0xFF) as usize];
    }
    crc ^ 0xFFFF_FFFF
}

/// Calculate CRC32 for a u32 value (little-endian)
#[inline]
pub fn crc32_u32(value: u32) -> u32 {
    crc32(&value.to_le_bytes())
}

/// Calculate CRC32 for a u64 value (little-endian)
#[inline]
pub fn crc32_u64(value: u64) -> u32 {
    crc32(&value.to_le_bytes())
}

/// Calculate CRC32 for a u16 value (little-endian)
#[inline]
pub fn crc32_u16(value: u16) -> u32 {
    crc32(&value.to_le_bytes())
}

// ============================================================================
// HLC (Hybrid Logical Clock)
// ============================================================================

/// Hybrid Logical Clock state
///
/// HLC = (physical_time_ns: u48, logical_counter: u16)
/// Stored as u64: upper 48 bits = physical time, lower 16 bits = logical counter
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HLC(u64);

impl HLC {
    /// Maximum physical time bits (48 bits = ~8925 years from epoch)
    const PHYS_BITS: u64 = 48;
    /// Logical counter bits (16 bits)
    const LOGIC_BITS: u64 = 16;
    /// Mask for physical time
    const PHYS_MASK: u64 = (1 << Self::PHYS_BITS) - 1;
    /// Mask for logical counter
    const LOGIC_MASK: u64 = (1 << Self::LOGIC_BITS) - 1;

    /// Create HLC from raw u64
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        HLC(raw)
    }

    /// Get raw u64 representation
    #[inline]
    pub const fn to_raw(self) -> u64 {
        self.0
    }

    /// Get physical time in nanoseconds since epoch
    #[inline]
    pub const fn physical_ns(&self) -> u64 {
        self.0 >> Self::LOGIC_BITS
    }

    /// Get logical counter
    #[inline]
    pub const fn logical(&self) -> u16 {
        (self.0 & Self::LOGIC_MASK) as u16
    }

    /// Create HLC from physical time and logical counter
    #[inline]
    pub const fn from_parts(physical_ns: u64, logical: u16) -> Self {
        HLC((physical_ns << Self::LOGIC_BITS) | (logical as u64))
    }

    /// Get current HLC timestamp from system time
    pub fn now() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_nanos() as u64;

        // Mask to 48 bits
        let phys = now & Self::PHYS_MASK;
        HLC::from_parts(phys, 0)
    }

    /// Tick the HLC, returning a new timestamp
    ///
    /// If physical time has advanced, reset logical counter to 0.
    /// Otherwise, increment logical counter.
    pub fn tick(&self) -> Self {
        let current_phys = Self::now().physical_ns();
        let my_phys = self.physical_ns();

        if current_phys > my_phys {
            HLC::from_parts(current_phys, 0)
        } else if current_phys == my_phys {
            HLC::from_parts(my_phys, self.logical().wrapping_add(1))
        } else {
            // Physical time went backwards (NTP adjustment), stay logical
            HLC::from_parts(my_phys, self.logical().wrapping_add(1))
        }
    }

    /// Update this HLC with a received timestamp (for distributed sync)
    pub fn update(&self, received: HLC) -> Self {
        let current = Self::now();
        let current_phys = current.physical_ns();
        let recv_phys = received.physical_ns();
        let my_phys = self.physical_ns();

        // max(physical) component
        let max_phys = current_phys.max(my_phys).max(recv_phys);

        // logical component based on relationship
        let new_logic = if max_phys == current_phys {
            current
                .logical()
                .max(self.logical())
                .max(received.logical())
                .wrapping_add(1)
        } else if max_phys == my_phys {
            self.logical().max(received.logical()).wrapping_add(1)
        } else {
            received.logical().wrapping_add(1)
        };

        HLC::from_parts(max_phys, new_logic)
    }
}

/// Thread-safe HLC generator with atomic counter
pub struct HLCGenerator {
    last_phys: AtomicU64,
    last_logic: AtomicU64,
}

impl HLCGenerator {
    /// Create a new HLC generator
    pub const fn new() -> Self {
        HLCGenerator {
            last_phys: AtomicU64::new(0),
            last_logic: AtomicU64::new(0),
        }
    }

    /// Generate a new HLC timestamp
    pub fn generate(&self) -> HLC {
        let current = HLC::now();
        let current_phys = current.physical_ns();

        // Atomically update physical time and logical counter
        let last_phys = self.last_phys.load(Ordering::Relaxed);

        if current_phys > last_phys {
            // Physical time advanced, reset
            self.last_phys.store(current_phys, Ordering::Relaxed);
            self.last_logic.store(0, Ordering::Relaxed);
            HLC::from_parts(current_phys, 0)
        } else {
            // Same physical time, increment logical
            let new_logic = self.last_logic.fetch_add(1, Ordering::Relaxed) + 1;
            if new_logic > HLC::LOGIC_MASK {
                // Logical counter overflow, advance physical
                let new_phys = current_phys + 1;
                self.last_phys.store(new_phys, Ordering::Relaxed);
                self.last_logic.store(0, Ordering::Relaxed);
                HLC::from_parts(new_phys, 0)
            } else {
                HLC::from_parts(current_phys, new_logic as u16)
            }
        }
    }
}

impl Default for HLCGenerator {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Varint Encoding/Decoding
// ============================================================================

/// Maximum bytes needed for a u64 varint (10 bytes)
pub const VARINT_MAX_BYTES: usize = 10;

/// Maximum bytes needed for a zigzag-encoded i64 (10 bytes)
pub const ZIGZAG_MAX_BYTES: usize = 10;

/// Encode a u64 as varint into the provided buffer
///
/// Returns the number of bytes written
///
/// # Safety
/// - `buf` must have at least VARINT_MAX_BYTES capacity
#[inline]
pub fn encode_varint(value: u64, buf: &mut [u8]) -> usize {
    let mut v = value;
    let mut i = 0;

    while v >= 0x80 {
        buf[i] = (v as u8) | 0x80;
        v >>= 7;
        i += 1;
    }

    buf[i] = v as u8;
    i + 1
}

/// Encode a u64 as varint, returning a fixed-size array
#[inline]
pub fn encode_varint_fixed(value: u64) -> [u8; VARINT_MAX_BYTES] {
    let mut buf = [0u8; VARINT_MAX_BYTES];
    let len = encode_varint(value, &mut buf);

    // Pad with continuation marker for fixed size
    // Actually, just return the buffer with remaining bytes as 0
    // The decoder will stop at the first byte without high bit
    let mut result = [0u8; VARINT_MAX_BYTES];
    result[..len].copy_from_slice(&buf[..len]);
    result
}

/// Decode a varint from a byte slice
///
/// Returns (value, bytes_consumed) or None if invalid/incomplete
#[inline]
pub fn decode_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;

    for (i, &byte) in buf.iter().take(VARINT_MAX_BYTES).enumerate() {
        result |= ((byte & 0x7F) as u64) << shift;

        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }

        shift += 7;

        // Check for overflow (more than 10 bytes or value too large)
        if shift >= 70 {
            return None;
        }
    }

    None // Incomplete varint
}

/// Encode an i64 using zigzag encoding followed by varint
///
/// Zigzag: maps signed integers to unsigned so small negatives are small positives
/// Formula: (n << 1) ^ (n >> 63)
#[inline]
pub fn encode_zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// Decode an i64 from zigzag-encoded varint
///
/// Inverse of zigzag: (n >> 1) ^ -((n & 1) as i64)
#[inline]
pub fn decode_zigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Encode an i64 as zigzag varint into buffer
///
/// Returns the number of bytes written
#[inline]
pub fn encode_zigzag_varint(value: i64, buf: &mut [u8]) -> usize {
    let encoded = encode_zigzag(value);
    encode_varint(encoded, buf)
}

/// Decode an i64 from zigzag varint in buffer
///
/// Returns (value, bytes_consumed) or None if invalid/incomplete
#[inline]
pub fn decode_zigzag_varint(buf: &[u8]) -> Option<(i64, usize)> {
    decode_varint(buf).map(|(v, len)| (decode_zigzag(v), len))
}

// ============================================================================
// Bit Packing Utilities
// ============================================================================

/// Block size for bit-packed values (128 elements per block)
pub const BITPACK_BLOCK_SIZE: usize = 128;

/// Header for a bit-packed block
///
/// Contains:
/// - base: u64 (the minimum value in the block, for delta encoding)
/// - bits: u8 (number of bits per element)
/// - count: u8 (actual number of elements, <= 128)
/// - reserved: u16
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct BitPackBlockHeader {
    pub base: u64,
    pub bits: u8,
    pub count: u8,
    pub reserved: u16,
}

impl BitPackBlockHeader {
    pub const SIZE: usize = std::mem::size_of::<Self>();

    /// Calculate the data size for a block with given bits per element
    pub const fn data_size(bits: u8, count: u8) -> usize {
        let total_bits = (bits as usize) * (count as usize);
        total_bits.div_ceil(8) // Round up to bytes
    }

    /// Calculate total block size (header + data)
    pub fn total_size(&self) -> usize {
        Self::SIZE + Self::data_size(self.bits, self.count)
    }
}

/// Encode a slice of u64 values into a bit-packed block
///
/// Uses delta encoding: stores (value - base) where base = min(values)
///
/// Returns the number of bytes written, or None if buffer too small
pub fn bitpack_encode(values: &[u64], buf: &mut [u8]) -> Option<usize> {
    if values.is_empty() || values.len() > BITPACK_BLOCK_SIZE {
        return None;
    }

    if buf.len() < BitPackBlockHeader::SIZE {
        return None;
    }

    // Find min and max to determine bits needed
    let base = *values.iter().min()?;
    let max_val = *values.iter().max()?;

    let max_delta = max_val - base;
    let bits_needed = if max_delta == 0 {
        1
    } else {
        64 - max_delta.leading_zeros() as u8
    };

    let count = values.len() as u8;

    let header = BitPackBlockHeader {
        base,
        bits: bits_needed,
        count,
        reserved: 0,
    };

    // Write header
    unsafe {
        std::ptr::write_unaligned(buf.as_mut_ptr() as *mut BitPackBlockHeader, header);
    }

    // Calculate data size
    let data_size = BitPackBlockHeader::data_size(bits_needed, count);
    let total_size = BitPackBlockHeader::SIZE + data_size;

    if buf.len() < total_size {
        return None;
    }

    // Pack values
    let data_start = BitPackBlockHeader::SIZE;
    let mut bit_offset = 0usize;
    let data_buf = &mut buf[data_start..data_start + data_size];

    // Zero the data buffer
    for byte in data_buf.iter_mut() {
        *byte = 0;
    }

    for &value in values {
        let delta = value - base;

        for bit_pos in 0..bits_needed {
            if (delta >> bit_pos) & 1 != 0 {
                let byte_idx = (bit_offset + bit_pos as usize) / 8;
                let bit_idx = (bit_offset + bit_pos as usize) % 8;
                if byte_idx < data_buf.len() {
                    data_buf[byte_idx] |= 1u8 << bit_idx;
                }
            }
        }

        bit_offset += bits_needed as usize;
    }

    Some(total_size)
}

/// Decode a bit-packed block into a slice of u64 values
///
/// Returns the number of values decoded, or None if buffer too small/invalid
pub fn bitpack_decode(buf: &[u8], output: &mut [u64]) -> Option<usize> {
    if buf.len() < BitPackBlockHeader::SIZE {
        return None;
    }

    // Read header using unsafe zero-copy
    let header = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const BitPackBlockHeader) };

    if header.bits == 0 || header.bits > 64 {
        return None;
    }

    if header.count as usize > output.len() {
        return None;
    }

    let data_size = BitPackBlockHeader::data_size(header.bits, header.count);
    let total_size = BitPackBlockHeader::SIZE + data_size;

    if buf.len() < total_size {
        return None;
    }

    let data_buf = &buf[BitPackBlockHeader::SIZE..];
    let mut bit_offset = 0usize;
    let mask = if header.bits == 64 {
        u64::MAX
    } else {
        (1u64 << header.bits) - 1
    };

    for out in output.iter_mut().take(header.count as usize) {
        let mut value = 0u64;

        for bit_pos in 0..header.bits {
            let byte_idx = (bit_offset + bit_pos as usize) / 8;
            let bit_idx = (bit_offset + bit_pos as usize) % 8;

            if byte_idx < data_buf.len() && (data_buf[byte_idx] >> bit_idx) & 1 != 0 {
                value |= 1u64 << bit_pos;
            }
        }

        *out = header.base + (value & mask);
        bit_offset += header.bits as usize;
    }

    Some(header.count as usize)
}

/// Encode delta values (differences between consecutive elements)
///
/// This is useful for sorted sequences where deltas are small
pub fn bitpack_encode_deltas(values: &[u64], buf: &mut [u8]) -> Option<usize> {
    if values.is_empty() {
        return None;
    }

    // Convert to deltas
    let mut deltas = Vec::with_capacity(values.len());
    deltas.push(values[0]); // First value is the base
    for i in 1..values.len() {
        deltas.push(values[i] - values[i - 1]);
    }

    bitpack_encode(&deltas, buf)
}

/// Decode delta-encoded bit-packed block
pub fn bitpack_decode_deltas(buf: &[u8], output: &mut [u64]) -> Option<usize> {
    let count = bitpack_decode(buf, output)?;

    // Convert deltas back to absolute values
    for i in 1..count {
        let prev = output[i - 1];
        output[i] += prev;
    }

    Some(count)
}

// ============================================================================
// Zero-copy helpers for aligned access
// ============================================================================

/// Safe wrapper for reading a u32 from bytes (little-endian)
#[inline]
pub fn read_u32_le(bytes: &[u8]) -> Option<u32> {
    bytes
        .get(0..4)
        .map(|arr| u32::from_le_bytes([arr[0], arr[1], arr[2], arr[3]]))
}

/// Safe wrapper for reading a u64 from bytes (little-endian)
#[inline]
pub fn read_u64_le(bytes: &[u8]) -> Option<u64> {
    bytes.get(0..8).map(|arr| {
        u64::from_le_bytes([
            arr[0], arr[1], arr[2], arr[3], arr[4], arr[5], arr[6], arr[7],
        ])
    })
}

/// Safe wrapper for reading a u16 from bytes (little-endian)
#[inline]
pub fn read_u16_le(bytes: &[u8]) -> Option<u16> {
    bytes
        .get(0..2)
        .map(|arr| u16::from_le_bytes([arr[0], arr[1]]))
}

/// Write u32 to bytes (little-endian)
#[inline]
pub fn write_u32_le(bytes: &mut [u8], value: u32) -> Option<()> {
    bytes.get_mut(0..4)?.copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// Write u64 to bytes (little-endian)
#[inline]
pub fn write_u64_le(bytes: &mut [u8], value: u64) -> Option<()> {
    bytes.get_mut(0..8)?.copy_from_slice(&value.to_le_bytes());
    Some(())
}

/// Write u16 to bytes (little-endian)
#[inline]
pub fn write_u16_le(bytes: &mut [u8], value: u16) -> Option<()> {
    bytes.get_mut(0..2)?.copy_from_slice(&value.to_le_bytes());
    Some(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc32() {
        let data = b"Hello, World!";
        let crc = crc32(data);
        assert_ne!(crc, 0);

        // Same data should produce same CRC
        assert_eq!(crc32(data), crc);

        // Different data should produce different CRC (with high probability)
        assert_ne!(crc32(b"Hello, World"), crc);
    }

    #[test]
    fn test_hlc_basic() {
        let hlc1 = HLC::now();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let hlc2 = HLC::now();

        assert!(hlc2.physical_ns() >= hlc1.physical_ns());
    }

    #[test]
    fn test_hlc_tick() {
        let hlc = HLC::from_parts(1000, 0);
        let hlc2 = hlc.tick();

        // Logical counter should increment
        assert!(hlc2.logical() >= hlc.logical());
    }

    #[test]
    fn test_varint_roundtrip() {
        let test_values = [0u64, 1, 127, 128, 255, 256, 16383, 16384, u64::MAX];
        let mut buf = [0u8; VARINT_MAX_BYTES];

        for &value in &test_values {
            let len = encode_varint(value, &mut buf);
            let (decoded, consumed) = decode_varint(&buf).unwrap();
            assert_eq!(value, decoded);
            assert_eq!(len, consumed);
        }
    }

    #[test]
    fn test_zigzag_roundtrip() {
        let test_values = [0i64, 1, -1, 100, -100, i64::MAX, i64::MIN];

        for &value in &test_values {
            let encoded = encode_zigzag(value);
            let decoded = decode_zigzag(encoded);
            assert_eq!(value, decoded);
        }
    }

    #[test]
    fn test_zigzag_varint_roundtrip() {
        let test_values = [0i64, 1, -1, 100, -100, 1000, -1000];
        let mut buf = [0u8; ZIGZAG_MAX_BYTES];

        for &value in &test_values {
            let _len = encode_zigzag_varint(value, &mut buf);
            let (decoded, consumed) = decode_zigzag_varint(&buf).unwrap();
            assert_eq!(value, decoded);
            assert!(consumed <= ZIGZAG_MAX_BYTES);
        }
    }

    #[test]
    fn test_bitpack_roundtrip() {
        let values = [100u64, 101, 102, 105, 110, 115, 120];
        let mut buf = [0u8; 256];
        let mut output = [0u64; 128];

        let len = bitpack_encode(&values, &mut buf).unwrap();
        let count = bitpack_decode(&buf[..len], &mut output).unwrap();

        assert_eq!(count, values.len());
        for (i, &v) in values.iter().enumerate() {
            assert_eq!(output[i], v);
        }
    }

    #[test]
    fn test_bitpack_delta_roundtrip() {
        let values = [100u64, 200, 300, 400, 500, 600, 700];
        let mut buf = [0u8; 256];
        let mut output = [0u64; 128];

        let len = bitpack_encode_deltas(&values, &mut buf).unwrap();
        let count = bitpack_decode_deltas(&buf[..len], &mut output).unwrap();

        assert_eq!(count, values.len());
        for (i, &v) in values.iter().enumerate() {
            assert_eq!(output[i], v);
        }
    }
}
