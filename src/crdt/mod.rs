//! CRDT (Conflict-free Replicated Data Type) implementations for MemoryX SKF-1.1.
//!
//! This module provides:
//! - GCounter: Grow-only counter with per-actor counts
//! - PNCounter: Positive-negative counter (GCounter - GCounter)
//! - LWWReg: Last-writer-wins register with HLC timestamps
//! - ORSet: Observed-remove set with add/remove dots
//! - ORMap: Observed-remove map with nested CRDTs
//! - WAL (Write-Ahead Log) for durability
//! - MetaStore for node metadata
//!
//! # WAL Record Format (SKF-1.1 Spec A.3.3)
//!
//! See `wal` module for 44-byte header format.

// CRDT submodules
pub mod snapshot;
pub mod wal;

pub use wal::{CrdtError, RecordHeader};
pub use wal::{KEY_KIND_ATOM, KEY_KIND_NODE};
pub use wal::{OP_MERGE_STATE, OP_REMOVE, OP_TOMBSTONE, OP_UPSERT};
pub use wal::{WalIterator, WalKey, WalReader, WalRecord, WalWriter};

pub use snapshot::{FieldRecordHeader, FieldState, NodeIterator, NodeState};
pub use snapshot::{IndexEntry, SnapshotBuilder, SnapshotHeader, SnapshotReader};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::hash::Hash;

use crate::store::{AtomId, CrdtKind, NodeNum};
use crate::utils::HLC;

// Re-export ActorId size for backward compatibility
pub use wal::ACTOR_ID_SIZE;

// ============================================================================
// Actor ID
// ============================================================================

/// Actor identifier for CRDT operations (16 bytes)
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ActorId(pub [u8; ACTOR_ID_SIZE]);

impl ActorId {
    /// Create a new ActorId from bytes
    #[inline]
    pub fn new(bytes: [u8; ACTOR_ID_SIZE]) -> Self {
        ActorId(bytes)
    }

    /// Generate a random ActorId
    pub fn generate() -> Self {
        let mut bytes = [0u8; ACTOR_ID_SIZE];
        // Simple random generation (use better RNG in production)
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (i as u8)
                ^ (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos() as u8);
        }
        ActorId(bytes)
    }

    /// Get bytes reference
    #[inline]
    pub fn as_bytes(&self) -> &[u8; ACTOR_ID_SIZE] {
        &self.0
    }

    /// Convert to bytes
    #[inline]
    pub fn into_bytes(self) -> [u8; ACTOR_ID_SIZE] {
        self.0
    }
}

impl fmt::Debug for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ActorId(")?;
        for byte in &self.0[..4] {
            write!(f, "{:02x}", byte)?;
        }
        write!(f, "...)")
    }
}

// ============================================================================
// Dot (for ORSet/ORMap)
// ============================================================================

/// A dot represents an actor's contribution to an ORSet element
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dot {
    pub actor: ActorId,
    pub counter: u64,
}

impl Dot {
    #[inline]
    pub fn new(actor: ActorId, counter: u64) -> Self {
        Dot { actor, counter }
    }
}

// ============================================================================
// GCounter (Grow-only Counter)
// ============================================================================

/// GCounter: Grow-only counter with per-actor counts
///
/// Each actor maintains their own count; total is sum of all counts.
#[derive(Debug, Clone, Default)]
pub struct GCounter {
    counts: HashMap<ActorId, u64>,
}

impl GCounter {
    /// Create a new empty GCounter
    #[inline]
    pub fn new() -> Self {
        GCounter {
            counts: HashMap::new(),
        }
    }

    /// Increment counter for an actor
    #[inline]
    pub fn inc(&mut self, actor: ActorId, delta: u64) {
        *self.counts.entry(actor).or_insert(0) += delta;
    }

    /// Get total count (sum of all actor counts)
    #[inline]
    pub fn value(&self) -> u64 {
        self.counts.values().sum()
    }

    /// Get count for a specific actor
    #[inline]
    pub fn get(&self, actor: &ActorId) -> u64 {
        *self.counts.get(actor).unwrap_or(&0)
    }

    /// Get all actor counts
    #[inline]
    pub fn counts(&self) -> &HashMap<ActorId, u64> {
        &self.counts
    }

    /// Merge another GCounter into this one (take max per actor)
    pub fn merge(&mut self, other: &GCounter) {
        for (actor, &count) in &other.counts {
            let entry = self.counts.entry(*actor).or_insert(0);
            *entry = (*entry).max(count);
        }
    }

    /// Serialize to bytes for WAL
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4 + self.counts.len() * (ACTOR_ID_SIZE + 8));

        // Write count of entries
        buf.extend_from_slice(&(self.counts.len() as u32).to_le_bytes());

        // Write each (actor, count) pair
        for (actor, count) in &self.counts {
            buf.extend_from_slice(&actor.0);
            buf.extend_from_slice(&count.to_le_bytes());
        }

        buf
    }

    /// Deserialize from bytes
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }

        let count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut counts = HashMap::with_capacity(count);

        let mut offset = 4;
        for _ in 0..count {
            if offset + ACTOR_ID_SIZE + 8 > data.len() {
                return None;
            }

            let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
            actor_bytes.copy_from_slice(&data[offset..offset + ACTOR_ID_SIZE]);
            offset += ACTOR_ID_SIZE;

            let count = u64::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
                data[offset + 7],
            ]);
            offset += 8;

            counts.insert(ActorId(actor_bytes), count);
        }

        Some(GCounter { counts })
    }
}

// ============================================================================
// PNCounter (Positive-Negative Counter)
// ============================================================================

/// PNCounter: Counter that can increment and decrement
///
/// Implemented as two GCounters: positive and negative.
/// Value = positive.value() - negative.value()
#[derive(Debug, Clone, Default)]
pub struct PNCounter {
    p: GCounter, // Positive counts
    n: GCounter, // Negative counts
}

impl PNCounter {
    /// Create a new empty PNCounter
    #[inline]
    pub fn new() -> Self {
        PNCounter {
            p: GCounter::new(),
            n: GCounter::new(),
        }
    }

    /// Increment counter
    #[inline]
    pub fn inc(&mut self, actor: ActorId, delta: u64) {
        self.p.inc(actor, delta);
    }

    /// Decrement counter
    #[inline]
    pub fn dec(&mut self, actor: ActorId, delta: u64) {
        self.n.inc(actor, delta);
    }

    /// Get current value (positive - negative)
    #[inline]
    pub fn value(&self) -> i64 {
        self.p.value() as i64 - self.n.value() as i64
    }

    /// Merge another PNCounter into this one
    pub fn merge(&mut self, other: &PNCounter) {
        self.p.merge(&other.p);
        self.n.merge(&other.n);
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let p_bytes = self.p.to_bytes();
        let n_bytes = self.n.to_bytes();

        let mut buf = Vec::with_capacity(4 + p_bytes.len() + 4 + n_bytes.len());
        buf.extend_from_slice(&(p_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&p_bytes);
        buf.extend_from_slice(&(n_bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(&n_bytes);

        buf
    }

    /// Deserialize from bytes
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }

        let p_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let p_bytes = &data[4..4 + p_len];

        let n_offset = 4 + p_len;
        if n_offset + 4 > data.len() {
            return None;
        }
        let n_len = u32::from_le_bytes([
            data[n_offset],
            data[n_offset + 1],
            data[n_offset + 2],
            data[n_offset + 3],
        ]) as usize;
        let n_bytes = &data[n_offset + 4..n_offset + 4 + n_len];

        Some(PNCounter {
            p: GCounter::from_bytes(p_bytes)?,
            n: GCounter::from_bytes(n_bytes)?,
        })
    }

    /// Get reference to positive GCounter
    #[inline]
    pub fn p_counts(&self) -> &GCounter {
        &self.p
    }

    /// Get reference to negative GCounter
    #[inline]
    pub fn n_counts(&self) -> &GCounter {
        &self.n
    }

    /// Create a PNCounter from two GCounters
    #[inline]
    pub fn from_gcounters(p: GCounter, n: GCounter) -> Self {
        PNCounter { p, n }
    }
}

// ============================================================================
// LWWReg (Last-Writer-Wins Register)
// ============================================================================

/// LWW Register with HLC timestamps
///
/// Layout:
/// - hlc_phys: u64 (8 bytes)
/// - hlc_logical: u32 (4 bytes)
/// - actor_id: [u8; 16] (16 bytes)
/// - value: bytes (variable)
#[derive(Debug, Clone)]
pub struct LWWReg<T> {
    hlc: HLC,
    actor: ActorId,
    value: T,
}

impl<T: Clone> LWWReg<T> {
    /// Create a new LWWReg with a value
    #[inline]
    pub fn new(hlc: HLC, actor: ActorId, value: T) -> Self {
        LWWReg { hlc, actor, value }
    }

    /// Update value if timestamp is newer
    pub fn update(&mut self, hlc: HLC, actor: ActorId, value: T) {
        if hlc >= self.hlc || (hlc == self.hlc && actor > self.actor) {
            self.hlc = hlc;
            self.actor = actor;
            self.value = value;
        }
    }

    /// Get current value
    #[inline]
    pub fn get(&self) -> &T {
        &self.value
    }

    /// Get timestamp
    #[inline]
    pub fn hlc(&self) -> HLC {
        self.hlc
    }

    /// Get actor
    #[inline]
    pub fn actor(&self) -> &ActorId {
        &self.actor
    }

    /// Merge another LWWReg (take newer value)
    pub fn merge(&mut self, other: &LWWReg<T>) {
        if other.hlc > self.hlc || (other.hlc == self.hlc && other.actor > self.actor) {
            self.hlc = other.hlc;
            self.actor = other.actor;
            self.value = other.value.clone();
        }
    }
}

impl<T: Clone + Default> Default for LWWReg<T> {
    fn default() -> Self {
        LWWReg {
            hlc: HLC::from_raw(0),
            actor: ActorId::new([0; ACTOR_ID_SIZE]),
            value: T::default(),
        }
    }
}

// ============================================================================
// ORSet (Observed-Remove Set)
// ============================================================================

/// Observed-Remove Set with add/remove dots
///
/// Elements are tagged with dots (actor, counter) when added.
/// Removal removes specific dots, not elements directly.
#[derive(Debug, Clone, Default)]
pub struct ORSet<T>
where
    T: Eq + Hash + Clone,
{
    /// Map from element to set of dots
    elements: HashMap<T, HashSet<Dot>>,
    /// Tombstones for removed dots
    tombstones: HashMap<T, HashSet<Dot>>,
    /// Actor counters for generating new dots
    counters: HashMap<ActorId, u64>,
}

impl<T> ORSet<T>
where
    T: Eq + Hash + Clone,
{
    /// Create a new empty ORSet
    #[inline]
    pub fn new() -> Self {
        ORSet {
            elements: HashMap::new(),
            tombstones: HashMap::new(),
            counters: HashMap::new(),
        }
    }

    /// Generate a new dot for an actor
    fn new_dot(&mut self, actor: ActorId) -> Dot {
        let counter = self.counters.entry(actor).or_insert(0);
        let dot = Dot::new(actor, *counter);
        *counter += 1;
        dot
    }

    /// Add an element
    pub fn add(&mut self, actor: ActorId, element: T) {
        let dot = self.new_dot(actor);
        self.elements.entry(element).or_default().insert(dot);
    }

    /// Remove an element (all current dots)
    pub fn remove(&mut self, actor: ActorId, element: &T) {
        if let Some(dots) = self.elements.get(element) {
            let dots_to_remove: HashSet<Dot> = dots.clone();
            for dot in dots_to_remove {
                self.tombstones
                    .entry(element.clone())
                    .or_default()
                    .insert(dot);
            }
        }
        // Also record this actor's intent to remove
        let _ = actor;
    }

    /// Check if element is present
    #[inline]
    pub fn contains(&self, element: &T) -> bool {
        if let Some(dots) = self.elements.get(element) {
            let tombstones = self.tombstones.get(element);
            dots.iter()
                .any(|dot| tombstones.map(|t| !t.contains(dot)).unwrap_or(true))
        } else {
            false
        }
    }

    /// Get all present elements
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.elements.iter().filter_map(|(elem, dots)| {
            let tombstones = self.tombstones.get(elem);
            let has_live_dot = dots
                .iter()
                .any(|dot| tombstones.map(|t| !t.contains(dot)).unwrap_or(true));
            if has_live_dot { Some(elem) } else { None }
        })
    }

    /// Check if set is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// Get element count
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Merge another ORSet into this one
    pub fn merge(&mut self, other: &ORSet<T>) {
        // Merge elements
        for (elem, dots) in &other.elements {
            self.elements.entry(elem.clone()).or_default().extend(dots);
        }

        // Merge tombstones
        for (elem, dots) in &other.tombstones {
            self.tombstones
                .entry(elem.clone())
                .or_default()
                .extend(dots);
        }

        // Update counters (take max)
        for (actor, &counter) in &other.counters {
            let entry = self.counters.entry(*actor).or_insert(0);
            *entry = (*entry).max(counter);
        }
    }

    /// Clear the set
    #[inline]
    pub fn clear(&mut self) {
        self.elements.clear();
        self.tombstones.clear();
    }

    /// Get reference to elements HashMap (for serialization)
    #[inline]
    pub fn elements_map(&self) -> &HashMap<T, HashSet<Dot>> {
        &self.elements
    }

    /// Get reference to tombstones HashMap (for serialization)
    #[inline]
    pub fn tombstones_map(&self) -> &HashMap<T, HashSet<Dot>> {
        &self.tombstones
    }

    /// Add a specific dot for an element (for deserialization)
    pub fn add_dot(&mut self, actor: ActorId, element: T, counter: u64) {
        let dot = Dot::new(actor, counter);
        self.elements.entry(element).or_default().insert(dot);

        // Update counters
        let current = self.counters.entry(actor).or_insert(0);
        *current = (*current).max(counter + 1);
    }

    /// Add a specific tombstone dot for an element (for deserialization)
    pub fn remove_dot(&mut self, actor: ActorId, element: T, counter: u64) {
        let dot = Dot::new(actor, counter);
        self.tombstones.entry(element).or_default().insert(dot);

        // Update counters
        let current = self.counters.entry(actor).or_insert(0);
        *current = (*current).max(counter + 1);
    }
}

// Specialized serialization for ORSet<Vec<u8>> (bytes elements)
impl ORSet<Vec<u8>> {
    /// Serialize ORSet<Vec<u8>> to bytes (SKF-1.1 format A.4)
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Get unique elements (union of elements and tombstones keys)
        let all_keys: HashSet<&Vec<u8>> =
            self.elements.keys().chain(self.tombstones.keys()).collect();
        buf.extend_from_slice(&(all_keys.len() as u32).to_le_bytes());

        for elem in all_keys {
            // Serialize element bytes
            buf.extend_from_slice(&(elem.len() as u32).to_le_bytes());
            buf.extend_from_slice(elem);

            // Add dots
            let add_dots = self.elements.get(elem).cloned().unwrap_or_default();
            buf.extend_from_slice(&(add_dots.len() as u32).to_le_bytes());
            for dot in &add_dots {
                buf.extend_from_slice(&dot.actor.0);
                buf.extend_from_slice(&dot.counter.to_le_bytes());
            }

            // Remove dots (tombstones)
            let rem_dots = self.tombstones.get(elem).cloned().unwrap_or_default();
            buf.extend_from_slice(&(rem_dots.len() as u32).to_le_bytes());
            for dot in &rem_dots {
                buf.extend_from_slice(&dot.actor.0);
                buf.extend_from_slice(&dot.counter.to_le_bytes());
            }
        }

        buf
    }

    /// Deserialize ORSet<Vec<u8>> from bytes (SKF-1.1 format A.4)
    pub fn from_bytes(data: &[u8]) -> Result<Self, CrdtError> {
        if data.len() < 4 {
            return Err(CrdtError::InvalidPayload);
        }

        let elem_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut set = ORSet::new();
        let mut offset = 4;

        for _ in 0..elem_count {
            // Read element
            if offset + 4 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let elem_len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            if offset + elem_len > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let elem = data[offset..offset + elem_len].to_vec();
            offset += elem_len;

            // Read add dots
            if offset + 4 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let add_count = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            for _ in 0..add_count {
                if offset + ACTOR_ID_SIZE + 8 > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
                actor_bytes.copy_from_slice(&data[offset..offset + ACTOR_ID_SIZE]);
                offset += ACTOR_ID_SIZE;
                let counter = u64::from_le_bytes(
                    data[offset..offset + 8]
                        .try_into()
                        .map_err(|_| CrdtError::InvalidPayload)?,
                );
                offset += 8;
                set.add_dot(ActorId(actor_bytes), elem.clone(), counter);
            }

            // Read remove dots
            if offset + 4 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let rem_count = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            for _ in 0..rem_count {
                if offset + ACTOR_ID_SIZE + 8 > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
                actor_bytes.copy_from_slice(&data[offset..offset + ACTOR_ID_SIZE]);
                offset += ACTOR_ID_SIZE;
                let counter = u64::from_le_bytes(
                    data[offset..offset + 8]
                        .try_into()
                        .map_err(|_| CrdtError::InvalidPayload)?,
                );
                offset += 8;
                set.remove_dot(ActorId(actor_bytes), elem.clone(), counter);
            }
        }

        Ok(set)
    }
}

// ============================================================================
// ORMap (Observed-Remove Map)
// ============================================================================

/// Observed-Remove Map with nested CRDTs
///
/// Each key maps to a nested CRDT value.
#[derive(Debug, Clone, Default)]
pub struct ORMap<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    entries: HashMap<K, LWWReg<V>>,
    removed: HashSet<K>,
}

impl<K, V> ORMap<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone + Default,
{
    /// Create a new empty ORMap
    #[inline]
    pub fn new() -> Self {
        ORMap {
            entries: HashMap::new(),
            removed: HashSet::new(),
        }
    }

    /// Get or create entry for a key
    pub fn get_or_create(&mut self, key: &K) -> &mut LWWReg<V> {
        self.removed.remove(key);
        self.entries.entry(key.clone()).or_default()
    }

    /// Update a key's value
    pub fn put(&mut self, hlc: HLC, actor: ActorId, key: K, value: V) {
        self.removed.remove(&key);
        let entry = self.entries.entry(key).or_default();
        entry.update(hlc, actor, value);
    }

    /// Remove a key
    pub fn remove(&mut self, key: &K) {
        self.removed.insert(key.clone());
    }

    /// Get value for a key
    #[inline]
    pub fn get(&self, key: &K) -> Option<&V> {
        if self.removed.contains(key) {
            return None;
        }
        self.entries.get(key).map(|reg| reg.get())
    }

    /// Check if key exists
    #[inline]
    pub fn contains_key(&self, key: &K) -> bool {
        !self.removed.contains(key) && self.entries.contains_key(key)
    }

    /// Iterate over entries
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().filter_map(|(k, v)| {
            if self.removed.contains(k) {
                None
            } else {
                Some((k, v.get()))
            }
        })
    }

    /// Merge another ORMap into this one
    pub fn merge(&mut self, other: &ORMap<K, V>) {
        for (key, reg) in &other.entries {
            if !self.removed.contains(key) {
                let entry = self.entries.entry(key.clone()).or_default();
                entry.merge(reg);
            }
        }

        // Removed keys: only remove if we don't have newer data
        for key in &other.removed {
            if !self.entries.contains_key(key) {
                self.removed.insert(key.clone());
            }
        }
    }

    /// Get entry count
    #[inline]
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Check if map is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// Clear the map
    #[inline]
    pub fn clear(&mut self) {
        self.entries.clear();
        self.removed.clear();
    }
}

// Specialized serialization for ORMap<String, Vec<u8>> (byte values)
impl ORMap<String, Vec<u8>> {
    /// Serialize ORMap<String, Vec<u8>> to bytes (SKF-1.1 format)
    /// Format:
    ///   u32 entry_count
    ///   for each: u32 key_len, key_utf8, u64 hlc_phys, u32 hlc_logical, actor[16], u32 val_len, val_bytes
    ///   u32 removed_count
    ///   for each: u32 key_len, key_utf8
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // Entries
        buf.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (key, reg) in &self.entries {
            let key_bytes = key.as_bytes();
            buf.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(key_bytes);

            // LWWReg: hlc_phys(8), hlc_logical(4), actor(16), value
            let hlc = reg.hlc();
            buf.extend_from_slice(&hlc.to_raw().to_le_bytes());
            buf.extend_from_slice(&(hlc.logical() as u32).to_le_bytes());
            buf.extend_from_slice(&reg.actor().0);
            let val_bytes = reg.get();
            buf.extend_from_slice(&(val_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(val_bytes);
        }

        // Removed keys
        buf.extend_from_slice(&(self.removed.len() as u32).to_le_bytes());
        for key in &self.removed {
            let key_bytes = key.as_bytes();
            buf.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(key_bytes);
        }

        buf
    }

    /// Deserialize ORMap<String, Vec<u8>> from bytes
    pub fn from_bytes(data: &[u8]) -> Result<Self, CrdtError> {
        if data.len() < 4 {
            return Err(CrdtError::InvalidPayload);
        }

        let mut map = ORMap::new();
        let mut offset = 4;

        let entry_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        for _ in 0..entry_count {
            // Key
            if offset + 4 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let key_len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            if offset + key_len > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let key = String::from_utf8(data[offset..offset + key_len].to_vec())
                .map_err(|_| CrdtError::InvalidPayload)?;
            offset += key_len;

            // LWWReg
            if offset + 8 + 4 + ACTOR_ID_SIZE > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let phys = u64::from_le_bytes(
                data[offset..offset + 8]
                    .try_into()
                    .map_err(|_| CrdtError::InvalidPayload)?,
            );
            offset += 8;
            let logical = u32::from_le_bytes(
                data[offset..offset + 4]
                    .try_into()
                    .map_err(|_| CrdtError::InvalidPayload)?,
            );
            offset += 4;
            let hlc = HLC::from_parts(phys, logical as u16);
            let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
            actor_bytes.copy_from_slice(&data[offset..offset + ACTOR_ID_SIZE]);
            offset += ACTOR_ID_SIZE;
            let actor = ActorId(actor_bytes);

            if offset + 4 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let val_len = u32::from_le_bytes(
                data[offset..offset + 4]
                    .try_into()
                    .map_err(|_| CrdtError::InvalidPayload)?,
            ) as usize;
            offset += 4;
            if offset + val_len > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let value = data[offset..offset + val_len].to_vec();
            offset += val_len;

            map.put(hlc, actor, key, value);
        }

        // Removed keys
        if offset + 4 > data.len() {
            return Err(CrdtError::InvalidPayload);
        }
        let rem_count = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;
        for _ in 0..rem_count {
            if offset + 4 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let key_len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            if offset + key_len > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let key = String::from_utf8(data[offset..offset + key_len].to_vec())
                .map_err(|_| CrdtError::InvalidPayload)?;
            offset += key_len;
            map.remove(&key);
        }

        Ok(map)
    }
}

// ============================================================================
// MVReg (Multi-Value Register)
// ============================================================================

/// Dot: causal marker for MVReg
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MVDot {
    pub actor: ActorId,
    pub counter: u64,
}

impl MVDot {
    #[inline]
    pub fn new(actor: ActorId, counter: u64) -> Self {
        MVDot { actor, counter }
    }
}

/// Check if dots_a is causally less-or-equal to dots_b
fn dots_causally_le(dots_a: &[MVDot], dots_b: &[MVDot]) -> bool {
    let mut has_strictly_less = false;
    // Build lookup for b
    let b_counters: std::collections::HashMap<ActorId, u64> =
        dots_b
            .iter()
            .fold(std::collections::HashMap::new(), |mut acc, d| {
                let entry = acc.entry(d.actor).or_insert(0);
                *entry = (*entry).max(d.counter);
                acc
            });
    // Also track a-only counters
    let a_counters: std::collections::HashMap<ActorId, u64> =
        dots_a
            .iter()
            .fold(std::collections::HashMap::new(), |mut acc, d| {
                let entry = acc.entry(d.actor).or_insert(0);
                *entry = (*entry).max(d.counter);
                acc
            });

    for (actor, a_val) in &a_counters {
        let b_val = b_counters.get(actor).copied().unwrap_or(0);
        if *a_val > b_val {
            return false; // a has a dot that b doesn't cover
        }
        if *a_val < b_val {
            has_strictly_less = true;
        }
    }
    // Check if b has dots not in a (for strict inequality when a is subset)
    if !has_strictly_less {
        for (actor, b_val) in &b_counters {
            let a_val = a_counters.get(actor).copied().unwrap_or(0);
            if a_val < *b_val {
                has_strictly_less = true;
                break;
            }
        }
    }
    has_strictly_less || a_counters.is_empty() && !b_counters.is_empty()
}

#[allow(dead_code)]
/// Check if dots_a is causally strictly less than dots_b
fn dots_causally_lt(dots_a: &[MVDot], dots_b: &[MVDot]) -> bool {
    dots_causally_le(dots_a, dots_b) && !dots_causally_le(dots_b, dots_a)
}

/// MVReg: multi-value register with causal ordering.
///
/// Stores concurrent values and resolves via causal ordering on merge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MVReg<V> {
    /// Each value with its set of causal dots
    values: Vec<(Vec<MVDot>, V)>,
    /// Clock vector (actor -> max counter seen)
    clock: std::collections::HashMap<u128, u64>,
}

impl<V: Clone + PartialEq + Default> MVReg<V> {
    /// Create new empty MVReg
    #[inline]
    pub fn new() -> Self {
        MVReg {
            values: Vec::new(),
            clock: std::collections::HashMap::new(),
        }
    }

    /// Set value for an actor (causal add)
    pub fn set(&mut self, actor: ActorId, value: V) {
        // Get counter for this actor
        let actor_key = u128::from_le_bytes(actor.0);
        let counter = self.clock.entry(actor_key).or_insert(0);
        *counter += 1;

        // Create new dot
        let new_dot = MVDot {
            actor,
            counter: *counter,
        };

        // Remove all values that are causally dominated by the new dot
        let new_dots = vec![new_dot];
        self.values
            .retain(|(dots, _)| !dots_causally_le(dots, &new_dots));

        // Check if any existing values are concurrent with the new value
        // If the new dot dominates all, replace; otherwise add concurrent value
        let all_dominated = self
            .values
            .iter()
            .all(|(dots, _)| dots_causally_le(dots, &new_dots));

        if all_dominated {
            self.values.clear();
        }

        self.values.push((new_dots, value));
    }

    /// Get all concurrent values (returns a Vec)
    #[inline]
    pub fn values(&self) -> Vec<V> {
        self.values.iter().map(|(_, v)| v.clone()).collect()
    }

    /// Iterator over values
    #[inline]
    pub fn iter_values(&self) -> impl Iterator<Item = &V> {
        self.values.iter().map(|(_, v)| v)
    }

    /// Iterator over (dots, value) pairs
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&[MVDot], &V)> {
        self.values.iter().map(|(dots, v)| (dots.as_slice(), v))
    }

    /// Number of concurrent values
    #[inline]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Check if empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Single value (only if singleton)
    #[inline]
    pub fn value(&self) -> Option<&V> {
        if self.values.len() == 1 {
            Some(&self.values[0].1)
        } else {
            None
        }
    }

    /// Merge two MVRegs using causal ordering
    pub fn merge(&mut self, other: MVReg<V>) {
        // Merge clocks (take max)
        for (&actor, &counter) in &other.clock {
            let entry = self.clock.entry(actor).or_insert(0);
            *entry = (*entry).max(counter);
        }

        // Collect all values from both
        let mut all_values: Vec<(Vec<MVDot>, V)> = std::mem::take(&mut self.values);
        let other_values = other.values;

        for (other_dots, other_val) in other_values {
            // Check if this value is dominated by any existing value
            let dominated = all_values.iter().any(|(dots, _)| {
                dots_causally_le(&other_dots, dots) && !dots_causally_le(dots, &other_dots)
            });
            if dominated {
                continue;
            }

            // Remove existing values dominated by this new value
            all_values.retain(|(dots, _)| !dots_causally_le(dots, &other_dots));

            // Add the new value
            all_values.push((other_dots, other_val));
        }

        self.values = all_values;
    }

    /// Serialize to bytes: u32 value_count, for each: u32 dot_count, dots[..], u32 value_len, value_bytes
    pub fn to_bytes(&self) -> Vec<u8>
    where
        V: serde::Serialize,
    {
        let mut buf = Vec::new();

        // value_count
        buf.extend_from_slice(&(self.values.len() as u32).to_le_bytes());

        for (dots, value) in &self.values {
            // dot_count
            buf.extend_from_slice(&(dots.len() as u32).to_le_bytes());

            // dots
            for dot in dots {
                buf.extend_from_slice(&dot.actor.0);
                buf.extend_from_slice(&dot.counter.to_le_bytes());
            }

            // value (Serde bincode-like: use serde_json for simplicity)
            let value_bytes = serde_json::to_vec(value).unwrap_or_default();
            buf.extend_from_slice(&(value_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&value_bytes);
        }

        // clock
        buf.extend_from_slice(&(self.clock.len() as u32).to_le_bytes());
        for (&actor, &counter) in &self.clock {
            buf.extend_from_slice(&actor.to_le_bytes());
            buf.extend_from_slice(&counter.to_le_bytes());
        }

        buf
    }

    /// Deserialize from bytes
    pub fn from_bytes(data: &[u8]) -> Result<Self, CrdtError>
    where
        V: serde::de::DeserializeOwned,
    {
        if data.len() < 4 {
            return Err(CrdtError::InvalidPayload);
        }

        let value_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let mut values = Vec::with_capacity(value_count);
        let mut offset = 4;

        for _ in 0..value_count {
            if offset + 4 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let dot_count = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;

            let mut dots = Vec::with_capacity(dot_count);
            for _ in 0..dot_count {
                if offset + ACTOR_ID_SIZE + 8 > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
                actor_bytes.copy_from_slice(&data[offset..offset + ACTOR_ID_SIZE]);
                offset += ACTOR_ID_SIZE;
                let counter = u64::from_le_bytes(
                    data[offset..offset + 8]
                        .try_into()
                        .map_err(|_| CrdtError::InvalidPayload)?,
                );
                offset += 8;
                dots.push(MVDot {
                    actor: ActorId(actor_bytes),
                    counter,
                });
            }

            if offset + 4 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let value_len = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            if offset + value_len > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let value: V = serde_json::from_slice(&data[offset..offset + value_len])
                .map_err(|_| CrdtError::InvalidPayload)?;
            offset += value_len;

            values.push((dots, value));
        }

        // Parse clock
        if offset + 4 > data.len() {
            return Err(CrdtError::InvalidPayload);
        }
        let clock_count = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;
        let mut clock = std::collections::HashMap::with_capacity(clock_count);
        for _ in 0..clock_count {
            if offset + 16 + 8 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let actor_key = u128::from_le_bytes(
                data[offset..offset + 16]
                    .try_into()
                    .map_err(|_| CrdtError::InvalidPayload)?,
            );
            offset += 16;
            let counter = u64::from_le_bytes(
                data[offset..offset + 8]
                    .try_into()
                    .map_err(|_| CrdtError::InvalidPayload)?,
            );
            offset += 8;
            clock.insert(actor_key, counter);
        }

        Ok(MVReg { values, clock })
    }
}

// ============================================================================
// FlagSet (LWW bitmask)
// ============================================================================

/// FlagSet: LWW bitmask for boolean flags.
///
/// Stores a u64 bitmask with HLC timestamp. On merge, the value
/// with the later timestamp wins (tie-break by actor comparison).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagSet {
    flags: u64,
    timestamp: HLC,
    actor: ActorId,
}

impl FlagSet {
    /// Create new empty FlagSet
    #[inline]
    pub fn new() -> Self {
        FlagSet {
            flags: 0,
            timestamp: HLC::from_raw(0),
            actor: ActorId::new([0u8; ACTOR_ID_SIZE]),
        }
    }

    /// Create FlagSet with initial values
    #[inline]
    pub fn with_flags(flags: u64, ts: HLC, actor: ActorId) -> Self {
        FlagSet {
            flags,
            timestamp: ts,
            actor,
        }
    }

    /// Set bits (OR)
    #[inline]
    pub fn set_bits(&mut self, mask: u64) {
        self.flags |= mask;
    }

    /// Clear bits (AND NOT)
    #[inline]
    pub fn clear_bits(&mut self, mask: u64) {
        self.flags &= !mask;
    }

    /// Get current flags
    #[inline]
    pub fn get(&self) -> u64 {
        self.flags
    }

    /// Set entire flags value (with timestamp update)
    #[inline]
    pub fn set(&mut self, flags: u64, ts: HLC, actor: ActorId) {
        if ts > self.timestamp || (ts == self.timestamp && actor > self.actor) {
            self.flags = flags;
            self.timestamp = ts;
            self.actor = actor;
        }
    }

    /// Check if flag bit is set
    #[inline]
    pub fn is_set(&self, bit: u8) -> bool {
        if bit >= 64 {
            return false;
        }
        (self.flags & (1u64 << bit)) != 0
    }

    /// Merge: LWW — who has newer timestamp wins (tie-break by actor)
    pub fn merge(&mut self, other: &FlagSet) {
        if other.timestamp > self.timestamp
            || (other.timestamp == self.timestamp && other.actor > self.actor)
        {
            self.flags = other.flags;
            self.timestamp = other.timestamp;
            self.actor = other.actor;
        }
    }

    /// Serialize: u64 flags + u64 HLC phys + u32 HLC logical + u128 actor
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + 8 + 4 + 16);
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf.extend_from_slice(&self.timestamp.to_raw().to_le_bytes());
        buf.extend_from_slice(&(self.timestamp.logical() as u32).to_le_bytes());
        buf.extend_from_slice(&self.actor.0);
        buf
    }

    /// Deserialize
    pub fn from_bytes(data: &[u8]) -> Result<Self, CrdtError> {
        if data.len() < 8 + 8 + 4 + 16 {
            return Err(CrdtError::InvalidPayload);
        }

        let flags = u64::from_le_bytes(
            data[0..8]
                .try_into()
                .map_err(|_| CrdtError::InvalidPayload)?,
        );
        let phys = u64::from_le_bytes(
            data[8..16]
                .try_into()
                .map_err(|_| CrdtError::InvalidPayload)?,
        );
        let logical = u32::from_le_bytes(
            data[16..20]
                .try_into()
                .map_err(|_| CrdtError::InvalidPayload)?,
        );
        let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
        actor_bytes.copy_from_slice(&data[20..36]);

        Ok(FlagSet {
            flags,
            timestamp: HLC::from_parts(phys, logical as u16),
            actor: ActorId(actor_bytes),
        })
    }
}

impl Default for FlagSet {
    fn default() -> Self {
        FlagSet::new()
    }
}

// ============================================================================
// CompactionScheduler
// ============================================================================

/// Configuration for CRDT WAL compaction
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// % WAL size relative to snapshot that triggers compaction (default 25)
    pub wal_size_threshold_pct: u8,
    /// Max number of WAL files before compaction (default 8)
    pub max_wal_files: u32,
    /// Max age of oldest WAL (hours) before compaction (default 24)
    pub max_wal_age_hours: u32,
    /// Tombstone retention (hours) (default 168 = 7 days)
    pub tombstone_retention_hours: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        CompactionConfig {
            wal_size_threshold_pct: 25,
            max_wal_files: 8,
            max_wal_age_hours: 24,
            tombstone_retention_hours: 168,
        }
    }
}

impl CompactionConfig {
    /// Create config with defaults
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Scheduler that determines when to compact WAL into snapshot
pub struct CompactionScheduler {
    config: CompactionConfig,
}

impl CompactionScheduler {
    /// Create scheduler with config
    #[inline]
    pub fn new(config: CompactionConfig) -> Self {
        CompactionScheduler { config }
    }

    /// Check if compaction should be triggered
    ///
    /// Triggers if ANY of:
    /// - wal_total_size > snapshot_size * threshold_pct / 100
    /// - wal_files.len() > max_wal_files
    pub fn should_compact(
        &self,
        wal_files: &[std::path::PathBuf],
        snapshot_size: u64,
        wal_total_size: u64,
    ) -> bool {
        // Check file count
        if wal_files.len() as u32 > self.config.max_wal_files {
            return true;
        }

        // Check size threshold
        if snapshot_size > 0 {
            let threshold = snapshot_size * self.config.wal_size_threshold_pct as u64 / 100;
            if wal_total_size > threshold {
                return true;
            }
        } else {
            // No snapshot yet, always compact if there are WAL files
            if !wal_files.is_empty() {
                return true;
            }
        }

        false
    }

    /// Check if WAL files are too old
    pub fn wals_too_old(&self, wal_files: &[std::path::PathBuf]) -> bool {
        if wal_files.is_empty() {
            return false;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let max_age = self.config.max_wal_age_hours as u64 * 3600;

        // Check oldest file
        if let Some(oldest) = wal_files.iter().min_by_key(|p| {
            p.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX)
        }) {
            let file_age = oldest
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| now.saturating_sub(d.as_secs()))
                .unwrap_or(0);
            file_age > max_age
        } else {
            false
        }
    }

    /// Compact WAL records into a snapshot-friendly format
    ///
    /// Algorithm:
    /// 1. Apply all WAL ops in HLC order
    /// 2. For each (node, field): CRDT join
    /// 3. Remove expired tombstones  
    /// 4. Return compacted state bytes
    pub fn compact(
        &self,
        wal_records: Vec<wal::WalRecord>,
        current_snapshot: Option<&MetaStore>,
        actor_id: ActorId,
    ) -> Result<MetaStore, CrdtError> {
        // Start from current snapshot or fresh store
        let mut store = current_snapshot
            .cloned()
            .unwrap_or_else(|| MetaStore::new(actor_id));

        // Sort WAL by HLC timestamp
        let mut sorted_records = wal_records;
        sorted_records.sort_by_key(|r| (r.header.hlc_phys_ns, r.header.hlc_logical));

        // Apply each WAL record
        for record in &sorted_records {
            let _hlc = HLC::from_parts(record.header.hlc_phys_ns, record.header.hlc_logical as u16);
            let _actor = ActorId::new(record.header.actor_id);
            let field_id = record.header.field_id;
            let crdt_kind = record.header.crdt_kind().ok_or(CrdtError::InvalidPayload)?;

            match record.key {
                wal::WalKey::Node(node_num) => {
                    let existing = store.get_node_crdt(node_num, field_id, crdt_kind);
                    // Create new state from payload
                    let new_state = deserialize_payload(crdt_kind, &record.payload)?;
                    existing.join(&new_state);
                }
                wal::WalKey::Atom(ref atom_id) => {
                    let existing = store.get_atom_crdt(atom_id, field_id, crdt_kind);
                    let new_state = deserialize_payload(crdt_kind, &record.payload)?;
                    existing.join(&new_state);
                }
            }
        }

        Ok(store)
    }

    /// Get config reference
    #[inline]
    pub fn config(&self) -> &CompactionConfig {
        &self.config
    }
}

/// Deserialize a CRDT state from payload bytes (public for federation sync)
pub fn deserialize_payload(kind: CrdtKind, data: &[u8]) -> Result<CrdtState, CrdtError> {
    let mut state = CrdtState::new(kind);
    match (&mut state, kind) {
        (CrdtState::GCounter(gc), CrdtKind::GCOUNTER) => {
            if let Some(parsed) = GCounter::from_bytes(data) {
                *gc = parsed;
            }
        }
        (CrdtState::PNCounter(pn), CrdtKind::PNCOUNTER) => {
            if let Some(parsed) = PNCounter::from_bytes(data) {
                *pn = parsed;
            }
        }
        (CrdtState::LWWReg(lww), CrdtKind::LWW_REG) => {
            // Parse LWWReg from bytes
            // Format: hlc_phys_ns (8) + hlc_logical (4) + actor (16) + value_len (4) + value
            if data.len() < 8 + 4 + ACTOR_ID_SIZE + 4 {
                return Err(CrdtError::InvalidPayload);
            }
            let phys = u64::from_le_bytes(
                data[0..8]
                    .try_into()
                    .map_err(|_| CrdtError::InvalidPayload)?,
            );
            let logical = u32::from_le_bytes(
                data[8..12]
                    .try_into()
                    .map_err(|_| CrdtError::InvalidPayload)?,
            );
            let hlc = HLC::from_parts(phys, logical as u16);
            let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
            actor_bytes.copy_from_slice(&data[12..12 + ACTOR_ID_SIZE]);
            let actor = ActorId(actor_bytes);
            let value_len = u32::from_le_bytes(
                data[12 + ACTOR_ID_SIZE..12 + ACTOR_ID_SIZE + 4]
                    .try_into()
                    .map_err(|_| CrdtError::InvalidPayload)?,
            ) as usize;
            if 12 + ACTOR_ID_SIZE + 4 + value_len > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let value = data[12 + ACTOR_ID_SIZE + 4..12 + ACTOR_ID_SIZE + 4 + value_len].to_vec();
            *lww = LWWReg::new(hlc, actor, value);
        }
        (CrdtState::ORSet(os), CrdtKind::ORSET) => {
            // ORSet decode (SKF-1.1 A.4)
            let parsed = ORSet::<Vec<u8>>::from_bytes(data)?;
            *os = parsed;
        }
        (CrdtState::ORMap(om), CrdtKind::ORMAP) => {
            // ORMap decode with nested CrdtKind and LWW timestamp preservation (SKF-1.1 A.2)
            // Format: u32 entry_count, for each: u32 key_len, key_utf8, u8 nested_kind,
            //          u64 hlc_phys, u32 hlc_logical, actor[16], u32 state_len, state_bytes
            //          u32 removed_count, for each: u32 key_len, key_utf8
            if data.len() < 4 {
                return Err(CrdtError::InvalidPayload);
            }
            let entry_count = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            let mut offset = 4;
            let mut entries: HashMap<String, LWWReg<CrdtState>> = HashMap::new();
            for _ in 0..entry_count {
                // Read key
                if offset + 4 > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let key_len = u32::from_le_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]) as usize;
                offset += 4;
                if offset + key_len > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let key = String::from_utf8(data[offset..offset + key_len].to_vec())
                    .map_err(|_| CrdtError::InvalidPayload)?;
                offset += key_len;
                // Read nested CrdtKind (CRITICAL: preserves nested type)
                if offset + 1 > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let nested_kind_byte = data[offset];
                offset += 1;
                let nested_kind =
                    CrdtKind::try_from(nested_kind_byte).map_err(|_| CrdtError::InvalidPayload)?;
                // CRITICAL: Read HLC and ActorId to preserve LWW semantics
                if offset + 8 + 4 + ACTOR_ID_SIZE > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let hlc_phys = u64::from_le_bytes(
                    data[offset..offset + 8]
                        .try_into()
                        .map_err(|_| CrdtError::InvalidPayload)?,
                );
                offset += 8;
                let hlc_logical = u32::from_le_bytes(
                    data[offset..offset + 4]
                        .try_into()
                        .map_err(|_| CrdtError::InvalidPayload)?,
                );
                offset += 4;
                let hlc = HLC::from_parts(hlc_phys, hlc_logical as u16);
                let mut actor_bytes = [0u8; ACTOR_ID_SIZE];
                actor_bytes.copy_from_slice(&data[offset..offset + ACTOR_ID_SIZE]);
                offset += ACTOR_ID_SIZE;
                let actor = ActorId(actor_bytes);
                // Read state_len
                if offset + 4 > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let state_len = u32::from_le_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]) as usize;
                offset += 4;
                if offset + state_len > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                // Recursively deserialize nested CRDT state
                let nested_state =
                    deserialize_payload(nested_kind, &data[offset..offset + state_len])?;
                offset += state_len;
                // CRITICAL: Restore LWWReg with original HLC and actor (preserves LWW semantics)
                let lww_reg = LWWReg::new(hlc, actor, nested_state);
                entries.insert(key, lww_reg);
            }
            // Read removed keys
            if offset + 4 > data.len() {
                return Err(CrdtError::InvalidPayload);
            }
            let rem_count = u32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            offset += 4;
            let mut removed: HashSet<String> = HashSet::new();
            for _ in 0..rem_count {
                if offset + 4 > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let key_len = u32::from_le_bytes([
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                ]) as usize;
                offset += 4;
                if offset + key_len > data.len() {
                    return Err(CrdtError::InvalidPayload);
                }
                let key = String::from_utf8(data[offset..offset + key_len].to_vec())
                    .map_err(|_| CrdtError::InvalidPayload)?;
                offset += key_len;
                removed.insert(key);
            }
            *om = ORMap { entries, removed };
        }
        (CrdtState::MVReg(mvr), CrdtKind::MVREG) => {
            // MVReg decode with concurrent values preservation
            let parsed = MVReg::<Vec<u8>>::from_bytes(data)?;
            *mvr = parsed;
        }
        (CrdtState::FlagSet(fs), CrdtKind::FLAGSET) => {
            let parsed = FlagSet::from_bytes(data)?;
            *fs = parsed;
        }
        _ => {}
    }
    Ok(state)
}

// ============================================================================
// CrdtState Enum (updated with MVReg and FlagSet)
// ============================================================================

/// Enum for different CRDT types
#[derive(Debug, Clone)]
pub enum CrdtState {
    /// GCounter
    GCounter(GCounter),
    /// PNCounter
    PNCounter(PNCounter),
    /// LWW Register (bytes)
    LWWReg(LWWReg<Vec<u8>>),
    /// ORSet (bytes)
    ORSet(ORSet<Vec<u8>>),
    /// ORMap
    ORMap(ORMap<String, CrdtState>),
    /// MVReg — multi-value register
    MVReg(MVReg<Vec<u8>>),
    /// FlagSet — LWW bitmask
    FlagSet(FlagSet),
}

impl Default for CrdtState {
    fn default() -> Self {
        CrdtState::GCounter(GCounter::new())
    }
}

impl CrdtState {
    /// Create a new CRDT of specified kind
    #[inline]
    pub fn new(kind: CrdtKind) -> Self {
        match kind {
            CrdtKind::GCOUNTER => CrdtState::GCounter(GCounter::new()),
            CrdtKind::PNCOUNTER => CrdtState::PNCounter(PNCounter::new()),
            CrdtKind::LWW_REG => CrdtState::LWWReg(LWWReg::default()),
            CrdtKind::ORSET => CrdtState::ORSet(ORSet::new()),
            CrdtKind::ORMAP => CrdtState::ORMap(ORMap::new()),
            CrdtKind::MVREG => CrdtState::MVReg(MVReg::new()),
            CrdtKind::FLAGSET => CrdtState::FlagSet(FlagSet::new()),
        }
    }

    /// Get CRDT kind
    #[inline]
    pub fn kind(&self) -> CrdtKind {
        match self {
            CrdtState::GCounter(_) => CrdtKind::GCOUNTER,
            CrdtState::PNCounter(_) => CrdtKind::PNCOUNTER,
            CrdtState::LWWReg(_) => CrdtKind::LWW_REG,
            CrdtState::ORSet(_) => CrdtKind::ORSET,
            CrdtState::ORMap(_) => CrdtKind::ORMAP,
            CrdtState::MVReg(_) => CrdtKind::MVREG,
            CrdtState::FlagSet(_) => CrdtKind::FLAGSET,
        }
    }

    /// Merge another CRDT state into this one
    pub fn join(&mut self, other: &CrdtState) -> bool {
        if self.kind() != other.kind() {
            return false;
        }

        match (self, other) {
            (CrdtState::GCounter(a), CrdtState::GCounter(b)) => {
                a.merge(b);
                true
            }
            (CrdtState::PNCounter(a), CrdtState::PNCounter(b)) => {
                a.merge(b);
                true
            }
            (CrdtState::LWWReg(a), CrdtState::LWWReg(b)) => {
                a.merge(b);
                true
            }
            (CrdtState::ORSet(a), CrdtState::ORSet(b)) => {
                a.merge(b);
                true
            }
            (CrdtState::ORMap(a), CrdtState::ORMap(b)) => {
                a.merge(b);
                true
            }
            (CrdtState::MVReg(a), CrdtState::MVReg(b)) => {
                let b_clone = b.clone();
                a.merge(b_clone);
                true
            }
            (CrdtState::FlagSet(a), CrdtState::FlagSet(b)) => {
                a.merge(b);
                true
            }
            _ => false,
        }
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            CrdtState::GCounter(c) => c.to_bytes(),
            CrdtState::PNCounter(c) => c.to_bytes(),
            CrdtState::LWWReg(r) => {
                // Format: hlc_phys_ns (8) + hlc_logical (4) + actor (16) + value_len (4) + value
                let value = r.get();
                let mut buf = Vec::with_capacity(8 + 4 + 16 + 4 + value.len());
                buf.extend_from_slice(&r.hlc().physical_ns().to_le_bytes());
                buf.extend_from_slice(&(r.hlc().logical() as u32).to_le_bytes());
                buf.extend_from_slice(&r.actor().0);
                buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
                buf.extend_from_slice(value);
                buf
            }
            CrdtState::ORSet(os) => {
                // ORSet serialization (SKF-1.1 A.4)
                os.to_bytes()
            }
            CrdtState::ORMap(om) => {
                // ORMap serialization with nested CrdtKind per value (SKF-1.1 A.2)
                // Format: u32 entry_count, for each: u32 key_len, key_utf8, u8 nested_kind,
                //          u64 hlc_phys, u32 hlc_logical, actor[16], u32 state_len, state_bytes
                //          u32 removed_count, for each: u32 key_len, key_utf8
                let mut buf = Vec::new();
                // Count actual entries (excluding removed keys that are in entries)
                let actual_entry_count = om
                    .entries
                    .keys()
                    .filter(|k| !om.removed.contains(*k))
                    .count();
                buf.extend_from_slice(&(actual_entry_count as u32).to_le_bytes());
                for (k, v) in om.entries.iter() {
                    // Skip removed keys in entries serialization
                    if om.removed.contains(k) {
                        continue;
                    }
                    let key_bytes = k.as_bytes();
                    buf.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
                    buf.extend_from_slice(key_bytes);
                    // Write nested CrdtKind BEFORE state_bytes (critical for roundtrip)
                    // v is &LWWReg<CrdtState>, need to get inner CrdtState via v.get()
                    buf.extend_from_slice(&[v.get().kind() as u8]);
                    // CRITICAL: Write HLC and ActorId from LWWReg wrapper to preserve LWW semantics
                    let hlc = v.hlc();
                    buf.extend_from_slice(&hlc.to_raw().to_le_bytes());
                    buf.extend_from_slice(&(hlc.logical() as u32).to_le_bytes());
                    buf.extend_from_slice(&v.actor().0);
                    // Write nested state bytes
                    let state_bytes = v.get().to_bytes();
                    buf.extend_from_slice(&(state_bytes.len() as u32).to_le_bytes());
                    buf.extend_from_slice(&state_bytes);
                }
                buf.extend_from_slice(&(om.removed.len() as u32).to_le_bytes());
                for k in &om.removed {
                    let key_bytes = k.as_bytes();
                    buf.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
                    buf.extend_from_slice(key_bytes);
                }
                buf
            }
            CrdtState::MVReg(m) => m.to_bytes(),
            CrdtState::FlagSet(f) => f.to_bytes(),
        }
    }
}

// ============================================================================
// MetaStore - Node metadata storage
// ============================================================================

/// Metadata store for node-level CRDTs
pub struct MetaStore {
    actor_id: ActorId,
    hlc: HLC,
    /// NodeNum -> Field -> CRDT state
    nodes: BTreeMap<NodeNum, BTreeMap<u16, CrdtState>>,
    /// AtomId -> Field -> CRDT state
    atoms: BTreeMap<AtomId, BTreeMap<u16, CrdtState>>,
}

impl Clone for MetaStore {
    fn clone(&self) -> Self {
        MetaStore {
            actor_id: self.actor_id,
            hlc: self.hlc,
            nodes: self.nodes.clone(),
            atoms: self.atoms.clone(),
        }
    }
}

impl MetaStore {
    /// Create a new MetaStore
    pub fn new(actor_id: ActorId) -> Self {
        MetaStore {
            actor_id,
            hlc: HLC::now(),
            nodes: BTreeMap::new(),
            atoms: BTreeMap::new(),
        }
    }

    /// Tick HLC
    #[inline]
    pub fn tick(&mut self) {
        self.hlc = self.hlc.tick();
    }

    /// Get or create a CRDT for a node field
    pub fn get_node_crdt(&mut self, node: NodeNum, field: u16, kind: CrdtKind) -> &mut CrdtState {
        let node_entry = self.nodes.entry(node).or_default();
        node_entry
            .entry(field)
            .or_insert_with(|| CrdtState::new(kind))
    }

    /// Get or create a CRDT for an atom field
    pub fn get_atom_crdt(&mut self, atom: &AtomId, field: u16, kind: CrdtKind) -> &mut CrdtState {
        let atom_entry = self.atoms.entry(*atom).or_default();
        atom_entry
            .entry(field)
            .or_insert_with(|| CrdtState::new(kind))
    }

    /// Apply a WAL record
    pub fn apply_wal_record(&mut self, header: &RecordHeader, payload: &[u8]) -> bool {
        if !header.is_valid() {
            return false;
        }

        let actor = ActorId::new(header.actor_id);
        let crdt_kind = match header.crdt_kind() {
            Some(k) => k,
            None => return false,
        };

        let state = CrdtState::new(crdt_kind);

        // Simplified: actual implementation would deserialize payload
        // and merge based on operation type

        match header.key_kind {
            KEY_KIND_NODE => {
                // Would decode NodeNum from payload
                let _ = actor;
                let _ = payload;
                let _ = state;
            }
            KEY_KIND_ATOM => {
                // Would decode AtomId from payload
                let _ = actor;
                let _ = payload;
                let _ = state;
            }
            _ => return false,
        }

        true
    }

    /// Merge state from another MetaStore
    pub fn merge(&mut self, other: &MetaStore) {
        // Merge nodes
        for (node, fields) in &other.nodes {
            let node_entry = self.nodes.entry(*node).or_default();
            for (field, state) in fields {
                let entry = match node_entry.entry(*field) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(state.clone())
                    }
                };
                entry.join(state);
            }
        }

        // Merge atoms
        for (atom, fields) in &other.atoms {
            let atom_entry = self.atoms.entry(*atom).or_default();
            for (field, state) in fields {
                let entry = match atom_entry.entry(*field) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(state.clone())
                    }
                };
                entry.join(state);
            }
        }
    }

    /// Get node count
    #[inline]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Get atom count
    #[inline]
    pub fn atom_count(&self) -> usize {
        self.atoms.len()
    }

    /// Get actor ID
    #[inline]
    pub fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }

    /// Get current HLC
    #[inline]
    pub fn hlc(&self) -> HLC {
        self.hlc
    }

    /// Export all node fields as iterator for federation sync
    /// Returns (NodeNum, field_id, crdt_kind, state_bytes) tuples
    pub fn export_node_fields(
        &self,
    ) -> impl Iterator<Item = (NodeNum, u16, crate::store::CrdtKind, Vec<u8>)> + '_ {
        self.nodes.iter().flat_map(|(node, fields)| {
            fields
                .iter()
                .map(move |(field_id, state)| (*node, *field_id, state.kind(), state.to_bytes()))
        })
    }

    /// Export all atom fields as iterator for federation sync
    /// Returns (AtomId, field_id, crdt_kind, state_bytes) tuples
    pub fn export_atom_fields(
        &self,
    ) -> impl Iterator<Item = (AtomId, u16, crate::store::CrdtKind, Vec<u8>)> + '_ {
        self.atoms.iter().flat_map(|(atom, fields)| {
            fields
                .iter()
                .map(move |(field_id, state)| (*atom, *field_id, state.kind(), state.to_bytes()))
        })
    }

    /// Import and merge a single node field from remote
    /// Performs real CRDT join (not overwrite)
    pub fn import_node_field(
        &mut self,
        node: NodeNum,
        field_id: u16,
        crdt_kind: crate::store::CrdtKind,
        state_bytes: &[u8],
    ) -> Result<bool, CrdtError> {
        // Deserialize remote state
        let remote_state = deserialize_payload(crdt_kind, state_bytes)?;

        // Get or create local CRDT
        let local_crdt = self.get_node_crdt(node, field_id, crdt_kind);

        // Perform CRDT join
        Ok(local_crdt.join(&remote_state))
    }

    /// Import and merge a single atom field from remote
    /// Performs real CRDT join (not overwrite)
    pub fn import_atom_field(
        &mut self,
        atom: &AtomId,
        field_id: u16,
        crdt_kind: crate::store::CrdtKind,
        state_bytes: &[u8],
    ) -> Result<bool, CrdtError> {
        // Deserialize remote state
        let remote_state = deserialize_payload(crdt_kind, state_bytes)?;

        // Get or create local CRDT
        let local_crdt = self.get_atom_crdt(atom, field_id, crdt_kind);

        // Perform CRDT join
        Ok(local_crdt.join(&remote_state))
    }

    /// Check if node has any fields
    #[inline]
    pub fn has_node(&self, node: NodeNum) -> bool {
        self.nodes.contains_key(&node)
    }

    /// Check if atom has any fields
    #[inline]
    pub fn has_atom(&self, atom: &AtomId) -> bool {
        self.atoms.contains_key(atom)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wal_header_size() {
        // The serialized size must be 50 bytes (actual field sum)
        assert_eq!(RecordHeader::SIZE, 50);
    }

    #[test]
    fn test_actor_id() {
        let actor = ActorId::generate();
        assert_eq!(actor.as_bytes().len(), ACTOR_ID_SIZE);

        let actor2 = ActorId::new([1u8; ACTOR_ID_SIZE]);
        assert_ne!(actor, actor2);
    }

    #[test]
    fn test_gcounter() {
        let mut counter = GCounter::new();
        let actor1 = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let actor2 = ActorId::new([2u8; ACTOR_ID_SIZE]);

        counter.inc(actor1, 10);
        counter.inc(actor2, 20);

        assert_eq!(counter.value(), 30);
        assert_eq!(counter.get(&actor1), 10);
        assert_eq!(counter.get(&actor2), 20);
    }

    #[test]
    fn test_gcounter_merge() {
        let mut counter1 = GCounter::new();
        let mut counter2 = GCounter::new();

        let actor1 = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let actor2 = ActorId::new([2u8; ACTOR_ID_SIZE]);

        counter1.inc(actor1, 10);
        counter1.inc(actor2, 5);

        counter2.inc(actor1, 15); // Higher count for actor1
        counter2.inc(actor2, 3); // Lower count for actor2

        counter1.merge(&counter2);

        assert_eq!(counter1.get(&actor1), 15); // Takes max
        assert_eq!(counter1.get(&actor2), 5); // Takes max
        assert_eq!(counter1.value(), 20);
    }

    #[test]
    fn test_pncounter() {
        let mut counter = PNCounter::new();
        let actor = ActorId::new([1u8; ACTOR_ID_SIZE]);

        counter.inc(actor, 100);
        assert_eq!(counter.value(), 100);

        counter.dec(actor, 30);
        assert_eq!(counter.value(), 70);

        counter.dec(actor, 100);
        assert_eq!(counter.value(), -30);
    }

    #[test]
    fn test_lwwreg() {
        let actor1 = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let actor2 = ActorId::new([2u8; ACTOR_ID_SIZE]);

        let hlc1 = HLC::from_parts(1000, 0);
        let hlc2 = HLC::from_parts(2000, 0); // Later

        let mut reg = LWWReg::new(hlc1, actor1, "value1".to_string());
        assert_eq!(reg.get(), "value1");

        // Update with newer timestamp
        reg.update(hlc2, actor2, "value2".to_string());
        assert_eq!(reg.get(), "value2");

        // Try to update with older timestamp (should not change)
        reg.update(hlc1, actor1, "value3".to_string());
        assert_eq!(reg.get(), "value2");
    }

    #[test]
    fn test_orset_basic() {
        let mut set = ORSet::new();
        let actor = ActorId::new([1u8; ACTOR_ID_SIZE]);

        set.add(actor, "apple".to_string());
        set.add(actor, "banana".to_string());

        assert!(set.contains(&"apple".to_string()));
        assert!(set.contains(&"banana".to_string()));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_orset_remove() {
        let mut set = ORSet::new();
        let actor = ActorId::new([1u8; ACTOR_ID_SIZE]);

        set.add(actor, "apple".to_string());
        assert!(set.contains(&"apple".to_string()));

        set.remove(actor, &"apple".to_string());
        assert!(!set.contains(&"apple".to_string()));
    }

    #[test]
    fn test_orset_merge() {
        let mut set1 = ORSet::new();
        let mut set2 = ORSet::new();

        let actor1 = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let actor2 = ActorId::new([2u8; ACTOR_ID_SIZE]);

        set1.add(actor1, "apple".to_string());
        set2.add(actor2, "banana".to_string());

        set1.merge(&set2);

        assert!(set1.contains(&"apple".to_string()));
        assert!(set1.contains(&"banana".to_string()));
        assert_eq!(set1.len(), 2);
    }

    #[test]
    fn test_ormap() {
        let mut map = ORMap::new();
        let actor = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let hlc = HLC::now();

        map.put(hlc, actor, "key1".to_string(), "value1".to_string());

        assert_eq!(map.get(&"key1".to_string()), Some(&"value1".to_string()));
        assert!(map.contains_key(&"key1".to_string()));

        map.remove(&"key1".to_string());
        assert_eq!(map.get(&"key1".to_string()), None);
    }

    #[test]
    fn test_crdt_state_enum() {
        let mut state1 = CrdtState::new(CrdtKind::GCOUNTER);
        let state2 = CrdtState::new(CrdtKind::GCOUNTER);

        assert_eq!(state1.kind(), CrdtKind::GCOUNTER);
        assert!(state1.join(&state2)); // Same kind
        assert!(!state1.join(&CrdtState::new(CrdtKind::PNCOUNTER))); // Different kind
    }

    #[test]
    fn test_wal_header_create() {
        let actor_id = [1u8; ACTOR_ID_SIZE];
        let hlc = HLC::from_parts(1000, 5);
        let payload = b"test data";

        let header = RecordHeader::new(
            hlc,
            actor_id,
            KEY_KIND_NODE,
            CrdtKind::GCOUNTER,
            0,
            OP_UPSERT,
            payload,
        );

        assert_eq!(header.magic, wal::WAL_MAGIC);
        assert_eq!(header.ver, wal::WAL_VERSION);
        assert_eq!(header.hlc_phys_ns, 1000);
        assert_eq!(header.hlc_logical, 5);
        assert_eq!(header.payload_len, 9);
        assert!(header.validate_crc(payload));
    }

    #[test]
    fn test_snapshot_builder() {
        use crate::crdt::snapshot::FieldState;

        let mut builder = SnapshotBuilder::new();

        let mut counter = GCounter::new();
        counter.inc(ActorId::new([1u8; ACTOR_ID_SIZE]), 100);
        let field = FieldState::from_gcounter(0, &counter);
        builder.add_node(1, vec![field]);

        assert_eq!(builder.node_count(), 1);

        let snapshot = builder.build();
        assert!(snapshot.len() > 4);
    }

    #[test]
    fn test_meta_store() {
        let actor = ActorId::generate();
        let mut store = MetaStore::new(actor);

        // Get/create CRDT for node
        let crdt = store.get_node_crdt(1, 0, CrdtKind::GCOUNTER);
        assert_eq!(crdt.kind(), CrdtKind::GCOUNTER);

        // Get/create CRDT for atom
        let atom_id = [42u8; 32];
        let crdt = store.get_atom_crdt(&atom_id, 0, CrdtKind::LWW_REG);
        assert_eq!(crdt.kind(), CrdtKind::LWW_REG);

        assert_eq!(store.actor_id(), &actor);
    }

    #[test]
    fn test_orset_roundtrip() {
        let mut set = ORSet::new();
        let actor1 = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let actor2 = ActorId::new([2u8; ACTOR_ID_SIZE]);

        set.add(actor1, b"elem1".to_vec());
        set.add(actor2, b"elem2".to_vec());
        set.add(actor1, b"elem3".to_vec());
        set.remove(actor1, &b"elem1".to_vec());

        // Serialize
        let bytes = set.to_bytes();
        assert!(!bytes.is_empty());

        // Deserialize
        let restored = ORSet::<Vec<u8>>::from_bytes(&bytes).expect("ORSet roundtrip failed");

        // elem1 should be tombstoned
        assert!(!restored.contains(&b"elem1".to_vec()));
        assert!(restored.contains(&b"elem2".to_vec()));
        assert!(restored.contains(&b"elem3".to_vec()));
        assert_eq!(restored.len(), 2);
    }

    #[test]
    fn test_ormap_roundtrip() {
        let mut map = ORMap::new();
        let actor = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let hlc1 = HLC::from_parts(1000, 0);
        let hlc2 = HLC::from_parts(2000, 0);

        map.put(hlc1, actor, "key1".to_string(), b"value1".to_vec());
        map.put(hlc2, actor, "key2".to_string(), b"value2".to_vec());
        map.remove(&"key1".to_string());

        // Serialize
        let bytes = map.to_bytes();
        assert!(!bytes.is_empty());

        // Deserialize
        let restored =
            ORMap::<String, Vec<u8>>::from_bytes(&bytes).expect("ORMap roundtrip failed");
        assert!(restored.get(&"key2".to_string()).is_some());
        assert!(restored.get(&"key1".to_string()).is_none()); // removed
        assert_eq!(restored.len(), 1);
    }

    #[test]
    fn test_mvreg_roundtrip() {
        let mut reg = MVReg::new();
        let actor1 = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let actor2 = ActorId::new([2u8; ACTOR_ID_SIZE]);

        reg.set(actor1, b"value1".to_vec());
        reg.set(actor2, b"value2".to_vec());

        // Should have concurrent values
        assert!(!reg.values().is_empty());

        // Serialize
        let bytes = reg.to_bytes();
        assert!(!bytes.is_empty());

        // Deserialize - must preserve concurrent values
        let restored = MVReg::<Vec<u8>>::from_bytes(&bytes)
            .unwrap_or_else(|_| panic!("MVReg roundtrip failed"));
        let vals = restored.values();
        assert!(!vals.is_empty());

        // Concurrent values preserved, NOT collapsed to LWW
        for v in &vals {
            assert!(v == &b"value1".to_vec() || v == &b"value2".to_vec());
        }
    }

    #[test]
    fn test_crdt_state_roundtrip_all_kinds() {
        // Test all normative CRDT kinds
        let kinds = vec![
            CrdtKind::GCOUNTER,
            CrdtKind::PNCOUNTER,
            CrdtKind::LWW_REG,
            CrdtKind::ORSET,
            CrdtKind::ORMAP,
            CrdtKind::MVREG,
            CrdtKind::FLAGSET,
        ];

        for kind in kinds {
            let state = CrdtState::new(kind);
            let bytes = state.to_bytes();
            // Round-trip via deserialize_payload
            let restored = deserialize_payload(kind, &bytes)
                .unwrap_or_else(|_| panic!("Roundtrip failed for {:?}", kind));
            assert_eq!(restored.kind(), kind);
        }
    }

    #[test]
    fn test_ormap_nested_mixed_crdt_roundtrip() {
        // Test ORMap with mixed nested CRDT types (SKF-1.1 A.2)
        // key1 -> GCounter, key2 -> ORSet, key3 -> MVReg, key4 -> FlagSet (removed)
        let actor = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let hlc = HLC::from_parts(1000, 0);

        // Build ORMap with nested CRDTs
        let mut outer_map = ORMap::<String, CrdtState>::new();

        // key1: nested GCounter
        let mut gcounter = GCounter::new();
        gcounter.inc(actor, 42);
        outer_map.put(
            hlc,
            actor,
            "key1".to_string(),
            CrdtState::GCounter(gcounter),
        );

        // key2: nested ORSet
        let mut orset = ORSet::new();
        orset.add(actor, b"elem1".to_vec());
        orset.add(actor, b"elem2".to_vec());
        outer_map.put(hlc, actor, "key2".to_string(), CrdtState::ORSet(orset));

        // key3: nested MVReg
        let mut mvreg = MVReg::new();
        mvreg.set(actor, b"concurrent_val1".to_vec());
        outer_map.put(hlc, actor, "key3".to_string(), CrdtState::MVReg(mvreg));

        // key4: nested FlagSet (then removed)
        let flagset = FlagSet::with_flags(0xFF, hlc, actor);
        outer_map.put(hlc, actor, "key4".to_string(), CrdtState::FlagSet(flagset));
        outer_map.remove(&"key4".to_string());

        // Create CrdtState::ORMap wrapper
        let state = CrdtState::ORMap(outer_map);

        // Serialize
        let bytes = state.to_bytes();
        assert!(!bytes.is_empty());

        // Deserialize via deserialize_payload
        let restored = deserialize_payload(CrdtKind::ORMAP, &bytes)
            .expect("ORMap mixed nested roundtrip failed");

        // Verify outer kind
        assert_eq!(restored.kind(), CrdtKind::ORMAP);

        // Verify nested types preserved
        let restored_map = match &restored {
            CrdtState::ORMap(om) => om,
            _ => panic!("Expected ORMap"),
        };

        // key1 should be GCounter
        let key1_state = restored_map.get(&"key1".to_string()).expect("key1 missing");
        assert_eq!(key1_state.kind(), CrdtKind::GCOUNTER);
        if let CrdtState::GCounter(gc) = key1_state {
            assert_eq!(gc.value(), 42);
        } else {
            panic!("key1 should be GCounter, got {:?}", key1_state.kind());
        }

        // key2 should be ORSet
        let key2_state = restored_map.get(&"key2".to_string()).expect("key2 missing");
        assert_eq!(key2_state.kind(), CrdtKind::ORSET);
        if let CrdtState::ORSet(os) = key2_state {
            assert!(os.contains(&b"elem1".to_vec()));
            assert!(os.contains(&b"elem2".to_vec()));
        } else {
            panic!("key2 should be ORSet, got {:?}", key2_state.kind());
        }

        // key3 should be MVReg
        let key3_state = restored_map.get(&"key3".to_string()).expect("key3 missing");
        assert_eq!(key3_state.kind(), CrdtKind::MVREG);
        if let CrdtState::MVReg(mvr) = key3_state {
            let vals = mvr.values();
            assert!(!vals.is_empty());
            assert!(vals.contains(&b"concurrent_val1".to_vec()));
        } else {
            panic!("key3 should be MVReg, got {:?}", key3_state.kind());
        }

        // key4 should be removed (not present)
        assert!(
            restored_map.get(&"key4".to_string()).is_none(),
            "key4 should be removed"
        );

        // Verify no nested types were collapsed to LWWReg
        // (this would fail if the old broken code was still in use)
    }

    #[test]
    fn test_ormap_lww_timestamp_preservation_concurrent_merge() {
        // Test that ORMap serialization preserves HLC/ActorId for LWW semantics
        // When two ORMap instances have the same key with different timestamps,
        // merge should select the value with greater timestamp (LWW rule)
        let actor1 = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let actor2 = ActorId::new([2u8; ACTOR_ID_SIZE]);

        // Create first ORMap with timestamp 1000
        let hlc1 = HLC::from_parts(1000, 0);
        let mut map1 = ORMap::<String, CrdtState>::new();
        let nested1 = CrdtState::LWWReg(LWWReg::new(hlc1, actor1, b"value_1000".to_vec()));
        map1.put(hlc1, actor1, "key".to_string(), nested1);

        // Create second ORMap with timestamp 2000 (greater)
        let hlc2 = HLC::from_parts(2000, 0);
        let mut map2 = ORMap::<String, CrdtState>::new();
        let nested2 = CrdtState::LWWReg(LWWReg::new(hlc2, actor2, b"value_2000".to_vec()));
        map2.put(hlc2, actor2, "key".to_string(), nested2);

        // Serialize both
        let state1 = CrdtState::ORMap(map1);
        let bytes1 = state1.to_bytes();
        let state2 = CrdtState::ORMap(map2);
        let bytes2 = state2.to_bytes();

        // Deserialize both
        let restored1 =
            deserialize_payload(CrdtKind::ORMAP, &bytes1).expect("Failed to deserialize map1");
        let restored2 =
            deserialize_payload(CrdtKind::ORMAP, &bytes2).expect("Failed to deserialize map2");

        // Merge restored maps (LWW should select value_2000)
        if let (CrdtState::ORMap(mut om1), CrdtState::ORMap(om2)) = (restored1, restored2) {
            om1.merge(&om2);

            // Get the value for "key"
            let merged_value = om1
                .get(&"key".to_string())
                .expect("key should exist after merge");

            // Verify LWW semantics: value from map2 (timestamp 2000) should win
            if let CrdtState::LWWReg(lww) = merged_value {
                let val = lww.get();
                assert_eq!(
                    val, b"value_2000",
                    "LWW merge should select value with greater timestamp (2000 > 1000)"
                );
                assert_eq!(
                    lww.hlc().physical_ns(),
                    2000,
                    "HLC timestamp should be preserved"
                );
            } else {
                panic!("Expected LWWReg, got {:?}", merged_value.kind());
            }
        } else {
            panic!("Expected ORMap states");
        }
    }

    #[test]
    fn test_ormap_nested_ormap_roundtrip() {
        // Nested ORMap: outer map contains inner ORMap as value
        let actor = ActorId::new([1u8; ACTOR_ID_SIZE]);
        let hlc = HLC::from_parts(1000, 0);

        // Create inner ORMap with nested GCounter
        let mut inner_map: ORMap<String, CrdtState> = ORMap::new();
        let mut gcounter = GCounter::new();
        gcounter.inc(actor, 42);
        inner_map.put(
            hlc,
            actor,
            "inner_key".to_string(),
            CrdtState::GCounter(gcounter),
        );

        // Create outer ORMap containing inner ORMap as value
        let mut outer_map: ORMap<String, CrdtState> = ORMap::new();
        outer_map.put(
            hlc,
            actor,
            "outer_key".to_string(),
            CrdtState::ORMap(inner_map),
        );

        // Wrap in CrdtState for serialization
        let state = CrdtState::ORMap(outer_map);

        // Roundtrip through serialization
        let bytes = state.to_bytes();
        assert!(!bytes.is_empty());

        let restored =
            deserialize_payload(CrdtKind::ORMAP, &bytes).expect("Nested ORMap roundtrip failed");

        // Verify outer kind preserved
        assert!(matches!(restored, CrdtState::ORMap(_)));
        assert_eq!(restored.kind(), CrdtKind::ORMAP);

        // Verify nested structure preserved
        let restored_outer = match &restored {
            CrdtState::ORMap(om) => om,
            _ => panic!("Expected ORMap"),
        };

        // Verify outer_key contains an ORMap
        let outer_value = restored_outer
            .get(&"outer_key".to_string())
            .expect("outer_key missing");
        assert_eq!(outer_value.kind(), CrdtKind::ORMAP);

        // Verify inner ORMap structure
        let inner_restored = match outer_value {
            CrdtState::ORMap(im) => im,
            _ => panic!("outer_key should contain ORMap"),
        };

        // Verify inner_key contains GCounter with value 42
        let inner_value = inner_restored
            .get(&"inner_key".to_string())
            .expect("inner_key missing");
        assert_eq!(inner_value.kind(), CrdtKind::GCOUNTER);

        if let CrdtState::GCounter(gc) = inner_value {
            assert_eq!(gc.value(), 42);
        } else {
            panic!("inner_key should be GCounter");
        }
    }

    #[test]
    fn test_ormap_edge_cases() {
        // Test HLC=0 edge case
        let actor_zero = ActorId::new([0u8; ACTOR_ID_SIZE]);
        let hlc_zero = HLC::from_parts(0, 0);

        // Create LWWReg with HLC=0 and ActorId=zero
        let state = CrdtState::LWWReg(LWWReg::new(hlc_zero, actor_zero, vec![1, 2, 3]));

        // Roundtrip should preserve HLC=0
        let bytes = state.to_bytes();
        assert!(!bytes.is_empty());

        let restored = deserialize_payload(CrdtKind::LWW_REG, &bytes)
            .expect("LWWReg with HLC=0 roundtrip failed");

        // Verify HLC phys == 0
        if let CrdtState::LWWReg(lww) = &restored {
            assert_eq!(
                lww.hlc().physical_ns(),
                0,
                "HLC phys should be preserved as 0"
            );
            assert_eq!(
                lww.hlc().logical(),
                0,
                "HLC logical should be preserved as 0"
            );
            assert_eq!(
                lww.actor().0,
                [0u8; ACTOR_ID_SIZE],
                "ActorId should be preserved as zero"
            );
            assert_eq!(lww.get(), &vec![1, 2, 3], "Value should be preserved");
        } else {
            panic!("Expected LWWReg, got {:?}", restored.kind());
        }

        // Test ORMap with HLC=0 entries
        let mut map: ORMap<String, CrdtState> = ORMap::new();
        map.put(
            hlc_zero,
            actor_zero,
            "zero_key".to_string(),
            CrdtState::GCounter(GCounter::new()),
        );

        let outer_state = CrdtState::ORMap(map);
        let bytes2 = outer_state.to_bytes();
        let restored2 = deserialize_payload(CrdtKind::ORMAP, &bytes2)
            .expect("ORMap with HLC=0 roundtrip failed");

        if let CrdtState::ORMap(om) = &restored2 {
            // Verify key with HLC=0 is preserved
            let value = om.get(&"zero_key".to_string());
            assert!(value.is_some(), "key with HLC=0 should be preserved");
        } else {
            panic!("Expected ORMap");
        }
    }
}
