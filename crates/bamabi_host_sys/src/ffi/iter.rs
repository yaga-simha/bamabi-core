//! Dense chunk and indirect sparse-row query iterator (`HostIter`).
//!
//! # Intent
//! Serializing ECS archetype tables across an FFI boundary on every frame destroys cache
//! locality and frame budgets. `HostIter` exposes Bevy's underlying `Table` columns directly
//! as contiguous Structure-of-Arrays (SoA) byte slices—and `SparseSet` columns as batched
//! per-row pointer arrays—without copying component payloads.
//!
//! # How It Works
//! 1. **Pre-resolved Metadata:** At system registration time, component handles are resolved
//!    once into [`QueryTermMeta`] entries containing [`ComponentId`], byte size/alignment,
//!    and storage flags.
//! 2. **Archetype Planning:** [`QueryPlan`] caches matching [`ArchetypeId`]s and table
//!    column indices so per-frame iteration avoids string hashing or repeated archetype scans.
//! 3. **Borrow-Checked `UnsafeWorldCell` Access:** [`HostIter`] holds an `UnsafeWorldCell`
//!    scoped to the duration of [`GuestSystem`](crate::system::GuestSystem) execution. Because
//!    `GuestSystem` registers all read/write [`ComponentId`]s in Bevy's `FilteredAccessSet`,
//!    Bevy's scheduler guarantees that no conflicting system runs concurrently.
//! 4. **Change Detection:** Mutable column accessors stamp Bevy's `changed` ticks with
//!    `this_run`, while [`TickFilterTerm`] evaluates `Added<T>` / `Changed<T>` predicates
//!    against `last_run` and `this_run`.

use crate::ffi::commands::{HostCommands, HostCommandsStorage};
use crate::handle_header::{
    HandleHeader, ITER_MAGIC, ffi_try, init_out_ptr, validate_iter_mut, validate_iter_ref,
};
use crate::registry::{MsgReadBatchFn, MsgSendFn, MsgWriteBatchFn};
use crate::sync_ptr::{GuestFnPtr, GuestUserData};
use bamabi_sys::{
    AccessMode, BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE,
    BAM_STORAGE_FLAG_SPARSE_SET, BAM_TERM_FLAG_ADDED, BAM_TERM_FLAG_CHANGED, BamChangeTicks,
    BamChunkTaskExFn, BamChunkTaskFn, BamColumnInfo, BamComponent, BamIter, BamParOptions,
    BamRowRange, ComponentUuid, FfiBool, FfiEntity, FfiResult, MAX_TERMS_PER_QUERY,
};
use bevy_ecs::archetype::{ArchetypeId, Archetypes};
use bevy_ecs::change_detection::{AtomicTick, Tick};
use bevy_ecs::component::ComponentId;
use bevy_ecs::entity::Entity;
use bevy_ecs::storage::{Column, SparseSets, Table, TableId, TableRow, Tables};
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy_tasks::{ComputeTaskPool, TaskPool};
use core::any::Any;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use nonmax::NonMaxU32;

// Compile-time layout assertions guarding zero-copy `Entity` -> `FfiEntity` slice casts.
const _: () = assert!(core::mem::size_of::<Entity>() == core::mem::size_of::<FfiEntity>());
const _: () = assert!(core::mem::align_of::<Entity>() == core::mem::align_of::<FfiEntity>());
const _: () = assert!(core::mem::size_of::<*mut u8>() == core::mem::size_of::<*const u8>());
const _: () = assert!(core::mem::size_of::<UnsafeCell<Tick>>() == core::mem::size_of::<u32>());
const _: () = assert!(core::mem::align_of::<UnsafeCell<Tick>>() == core::mem::align_of::<u32>());

/// Pre-resolved metadata for one term inside a `BamQueryDesc` (`0..MAX_TERMS_PER_QUERY`).
#[derive(Clone, Copy, Debug)]
pub struct QueryTermMeta {
    /// 1-based [`BamComponent`] handle.
    pub component: BamComponent,
    /// Bevy [`ComponentId`] assigned in the host `World`.
    pub component_id: ComponentId,
    /// Declared [`AccessMode`].
    pub mode: AccessMode,
    /// Term bitflags (`BAM_TERM_FLAG_*`).
    pub flags: u16,
    /// Registered 128-bit [`ComponentUuid`].
    pub uuid: ComponentUuid,
    /// Byte size (`size_of::<T>()`).
    pub size: usize,
    /// Byte alignment (`align_of::<T>()`).
    pub align: usize,
    /// Bitmask of `BAM_STORAGE_FLAG_*`.
    pub storage_flags: u32,
    /// Optional monomorphized batch message reader.
    pub msg_read_batch_fn: Option<MsgReadBatchFn>,
    /// Optional monomorphized message sender.
    pub msg_send_fn: Option<MsgSendFn>,
    /// Optional monomorphized batch message writer.
    pub msg_write_batch_fn: Option<MsgWriteBatchFn>,
}

impl QueryTermMeta {
    /// Returns `true` if [`BAM_TERM_FLAG_CHANGED`] is set or `mode == AccessMode::CHANGED`.
    #[inline]
    pub const fn has_changed_filter(&self) -> bool {
        (self.flags & BAM_TERM_FLAG_CHANGED) != 0 || self.mode.0 == AccessMode::CHANGED.0
    }

    /// Returns `true` if [`BAM_TERM_FLAG_ADDED`] is set or `mode == AccessMode::ADDED`.
    #[inline]
    pub const fn has_added_filter(&self) -> bool {
        (self.flags & BAM_TERM_FLAG_ADDED) != 0 || self.mode.0 == AccessMode::ADDED.0
    }

    /// Returns `true` if this term carries a `Changed<T>` or `Added<T>` tick filter.
    #[inline]
    pub const fn has_tick_filter(&self) -> bool {
        self.has_changed_filter() || self.has_added_filter()
    }

    /// Returns `true` if this term uses `StorageType::SparseSet`.
    #[inline]
    pub const fn is_sparse_set(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_SPARSE_SET) != 0
    }

    /// Returns `true` if this term is immutable (`BAM_STORAGE_FLAG_IMMUTABLE`).
    #[inline]
    pub const fn is_immutable(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_IMMUTABLE) != 0
    }

    /// Returns `true` if this term refers to a singleton Bevy `Resource`.
    #[inline]
    pub const fn is_resource(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_RESOURCE) != 0
    }

    /// Returns `true` if this term refers to a buffered Bevy `Messages<M>` stream.
    #[inline]
    pub const fn is_message(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_MESSAGE) != 0
    }
}

/// Precomputed tick-filter term (`Changed<T>` or `Added<T>`) resolved at `GuestSystem::initialize`.
#[derive(Clone, Copy, Debug)]
pub struct TickFilterTerm {
    /// Target Bevy [`ComponentId`].
    pub component_id: ComponentId,
    /// `true` for `AccessMode::ADDED`, `false` for `AccessMode::CHANGED`.
    pub is_added: bool,
    /// `true` if stored in `SparseSets` rather than `Table`.
    pub is_sparse: bool,
}

/// Precomputed filter clause inside a disjunctive `Or<(...)>` group (`or_group != 0`).
#[derive(Clone, Copy, Debug)]
pub struct OrFilterClause {
    /// Target Bevy [`ComponentId`].
    pub component_id: ComponentId,
    /// Filter access mode (`FILTER_ONLY`, `WITHOUT`, `CHANGED`, or `ADDED`).
    pub mode: AccessMode,
    /// Term bitflags (`BAM_TERM_FLAG_*`).
    pub flags: u16,
    /// `true` if stored in `SparseSets` rather than `Table`.
    pub is_sparse: bool,
}

impl OrFilterClause {
    /// Returns `true` if [`BAM_TERM_FLAG_CHANGED`] is set or `mode == AccessMode::CHANGED`.
    #[inline]
    pub const fn has_changed_filter(&self) -> bool {
        (self.flags & BAM_TERM_FLAG_CHANGED) != 0 || self.mode.0 == AccessMode::CHANGED.0
    }

    /// Returns `true` if [`BAM_TERM_FLAG_ADDED`] is set or `mode == AccessMode::ADDED`.
    #[inline]
    pub const fn has_added_filter(&self) -> bool {
        (self.flags & BAM_TERM_FLAG_ADDED) != 0 || self.mode.0 == AccessMode::ADDED.0
    }

    /// Returns `true` if this clause carries a `Changed<T>` or `Added<T>` tick filter.
    #[inline]
    pub const fn has_tick_filter(&self) -> bool {
        self.has_changed_filter() || self.has_added_filter()
    }
}

/// Disjunctive `Or<(...)>` filter group (`group_id != 0`) composed of [`OrFilterClause`]s.
#[derive(Clone, Debug)]
pub struct OrFilterGroup {
    /// Non-zero group identifier (`1..=255`).
    pub group_id: u8,
    /// Member clauses combined via logical OR.
    pub terms: Vec<OrFilterClause>,
}

/// Precomputed static layout and term plan for a `BamQueryDesc`.
#[derive(Clone, Debug, Default)]
pub struct QueryPlan {
    /// Term metadata in declaration order (`0..term_count`).
    pub terms: Vec<QueryTermMeta>,
    /// Precomputed indices (`0..term_count`) of dense `Table` data columns.
    pub dense_data_indices: Vec<u8>,
    /// Precomputed indices (`0..term_count`) of `AccessMode::HAS` presence indicator terms.
    pub has_indices: Vec<u8>,
    /// `true` when at least one `AccessMode::HAS` term is backed by `StorageType::SparseSet`.
    pub has_sparse_term: bool,
    /// Bitmask of `term_index`es backed by `StorageType::SparseSet` with entity column data.
    pub sparse_data_mask: u32,
    /// Bitmask of `term_index`es backed by `StorageType::Table` with entity column data.
    pub dense_data_mask: u32,
    /// Bitmask of `term_index`es assigned to a non-zero `or_group`.
    pub or_term_mask: u32,
    /// Precomputed top-level (`or_group == 0`) `Changed<T>` / `Added<T>` filter terms.
    pub tick_filters: Vec<TickFilterTerm>,
    /// Precomputed disjunctive `Or<(...)>` filter groups (`or_group != 0`).
    pub or_groups: Vec<OrFilterGroup>,
    /// `true` when `terms.is_empty()` or at least one term is an entity term
    /// (`!is_resource() && !is_message() && !is_removed_read()`).
    pub has_entity_terms: bool,
}

impl QueryPlan {
    /// Builds a [`QueryPlan`] directly from a slice of [`QueryTermMeta`]s with all `or_group = 0`
    /// (useful for unit tests as well as `GuestSystem::initialize`).
    pub fn from_terms(terms: Vec<QueryTermMeta>) -> Self {
        Self::from_terms_with_or_groups(terms, &[])
    }

    /// Builds a [`QueryPlan`] from [`QueryTermMeta`]s and parallel per-term `or_group` IDs.
    pub fn from_terms_with_or_groups(terms: Vec<QueryTermMeta>, term_or_groups: &[u8]) -> Self {
        let mut dense_data_indices = Vec::new();
        let mut has_indices = Vec::new();
        let mut has_sparse_term = false;
        let mut sparse_data_mask = 0u32;
        let mut dense_data_mask = 0u32;
        let mut or_term_mask = 0u32;
        let mut tick_filters = Vec::new();
        let mut or_groups: Vec<OrFilterGroup> = Vec::new();
        let has_entity_terms = terms.is_empty()
            || terms.iter().any(|t| {
                !t.mode.is_resource() && !t.mode.is_message() && !t.mode.is_removed_read()
            });

        for (i, t) in terms.iter().enumerate().take(MAX_TERMS_PER_QUERY) {
            let or_group = term_or_groups.get(i).copied().unwrap_or(0);
            if or_group != 0 {
                or_term_mask |= 1u32 << i;
                let clause = OrFilterClause {
                    component_id: t.component_id,
                    mode: t.mode,
                    flags: t.flags,
                    is_sparse: t.is_sparse_set(),
                };
                if let Some(group) = or_groups.iter_mut().find(|g| g.group_id == or_group) {
                    group.terms.push(clause);
                } else {
                    or_groups.push(OrFilterGroup {
                        group_id: or_group,
                        terms: vec![clause],
                    });
                }
                continue;
            }

            if t.mode.is_has() && !t.is_resource() && !t.is_message() {
                has_indices.push(i as u8);
                if t.is_sparse_set() {
                    has_sparse_term = true;
                }
                continue;
            }

            if !t.is_resource() && !t.is_message() {
                let mut push_tick = |is_added: bool| {
                    if !tick_filters.iter().any(|tf: &TickFilterTerm| {
                        tf.component_id == t.component_id && tf.is_added == is_added
                    }) {
                        tick_filters.push(TickFilterTerm {
                            component_id: t.component_id,
                            is_added,
                            is_sparse: t.is_sparse_set(),
                        });
                    }
                };
                if t.has_changed_filter() {
                    push_tick(false);
                }
                if t.has_added_filter() {
                    push_tick(true);
                }
            }

            if !t.mode.has_entity_column_data() || t.is_resource() || t.is_message() {
                continue;
            }

            // If a tick filter (`Changed<T>` / `Added<T>`) shares a `component_id` with a
            // `WRITE` / `OPTIONAL_WRITE` term in the same query, suppress column data on the
            // read-filter term so both cannot alias `&mut T` and `&T` simultaneously.
            if t.mode.is_tick_filter()
                && terms.iter().enumerate().any(|(j, other)| {
                    term_or_groups.get(j).copied().unwrap_or(0) == 0
                        && other.component_id == t.component_id
                        && (other.mode == AccessMode::WRITE
                            || other.mode == AccessMode::OPTIONAL_WRITE)
                })
            {
                continue;
            }

            if t.is_sparse_set() {
                sparse_data_mask |= 1u32 << i;
            } else {
                dense_data_mask |= 1u32 << i;
                dense_data_indices.push(i as u8);
            }
        }

        Self {
            terms,
            dense_data_indices,
            has_indices,
            has_sparse_term,
            sparse_data_mask,
            dense_data_mask,
            or_term_mask,
            tick_filters,
            or_groups,
            has_entity_terms,
        }
    }
}

/// Per-query-group reusable scratch storage for indirect sparse-set row pointer batching,
/// active column metadata, cached tick filters, `Has<T>` presence buffers, `msg_read_slice`
/// staging, and parallel chunk partition reuse.
#[derive(Default)]
pub struct SparseRowScratch {
    entities: Vec<FfiEntity>,
    row_ptrs: Vec<Vec<*mut u8>>,
    tick_ptrs: Vec<Vec<*const UnsafeCell<Tick>>>,
    active_cols: Vec<Option<RawActiveDenseColumn>>,
    cached_table_cols: Vec<Option<core::ptr::NonNull<Column>>>,
    filter_scratch: Option<Box<FilterScratch>>,
    has_true_buf: Vec<FfiBool>,
    has_false_buf: Vec<FfiBool>,
    msg_slice_buf: Vec<u8>,
    par_dense_partitions: Vec<DenseParPartition>,
    par_arch_partitions: Vec<ArchetypeParPartition>,
    par_worker_storages: Vec<HostCommandsStorage>,
    par_worker_scratches: Vec<SparseRowScratch>,
}

// SAFETY: `SparseRowScratch` holds temporary pointers only during an active single-threaded
// `GuestSystem::run_unsafe` chunk; all slices are cleared/overwritten before dereferencing and
// never accessed across threads.
unsafe impl Send for SparseRowScratch {}
// SAFETY: Shared references to `GuestSystem` never read or dereference `SparseRowScratch`.
unsafe impl Sync for SparseRowScratch {}

impl SparseRowScratch {
    /// Ensures inner per-term vectors cover `term_count` terms.
    #[inline]
    pub fn ensure_terms(&mut self, term_count: usize) {
        if self.row_ptrs.len() < term_count {
            self.row_ptrs.resize_with(term_count, Vec::new);
            self.tick_ptrs.resize_with(term_count, Vec::new);
        }
        if self.active_cols.len() < term_count {
            self.active_cols.resize(term_count, None);
        }
        if self.cached_table_cols.len() < term_count {
            self.cached_table_cols.resize(term_count, None);
        }
    }
}

#[derive(Clone, Copy)]
struct RunTicks {
    last_run: Tick,
    this_run: Tick,
}

#[derive(Clone, Copy)]
struct RawActiveDenseColumn {
    data_ptr: core::ptr::NonNull<u8>,
    col: Option<core::ptr::NonNull<Column>>,
    changed_ticks_ptr: *const UnsafeCell<Tick>,
    summary_tick: Option<core::ptr::NonNull<AtomicTick>>,
}

const _: () = assert!(core::mem::size_of::<Option<RawActiveDenseColumn>>() <= 32);

enum IterBacking<'w> {
    ResourceOnly,
    DenseTables {
        matched_tables: &'w [TableId],
        table_cursor: usize,
        row_cursor: usize,
        single_table_end_row: Option<usize>,
    },
    Archetypes {
        archetypes: &'w Archetypes,
        sparse_sets: &'w SparseSets,
        matched_archetypes: &'w [ArchetypeId],
        archetype_cursor: usize,
        entity_cursor: usize,
        single_arch_end_entity: Option<usize>,
    },
}

const MAX_TICK_FILTER_SLICES: usize = MAX_TERMS_PER_QUERY * 2;

#[derive(Clone, Copy)]
struct RawTickSlice {
    ptr: *const UnsafeCell<Tick>,
    len: usize,
}

impl RawTickSlice {
    const EMPTY: Self = Self {
        ptr: core::ptr::null(),
        len: 0,
    };

    #[inline(always)]
    const fn from_slice(slice: &[UnsafeCell<Tick>]) -> Self {
        Self {
            ptr: slice.as_ptr(),
            len: slice.len(),
        }
    }

    /// Reconstructs `&'t [UnsafeCell<Tick>]` from raw pointer and length.
    ///
    /// # Safety
    /// Caller must guarantee the backing `Table` column tick slice remains valid and
    /// immutably borrowed for `'t` (or `len == 0`).
    #[inline(always)]
    unsafe fn as_slice<'t>(self) -> &'t [UnsafeCell<Tick>] {
        if self.len == 0 || self.ptr.is_null() {
            &[]
        } else {
            // SAFETY: Caller guarantees `self.ptr..self.ptr + self.len` is live and valid for `'t`.
            unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

#[derive(Clone, Copy)]
struct RawDenseTableFilters {
    skip: bool,
    filter_slices: [RawTickSlice; MAX_TICK_FILTER_SLICES],
    filter_len: usize,
    or_slices: [RawTickSlice; MAX_TICK_FILTER_SLICES],
    or_slice_len: usize,
    or_ranges: [(usize, usize); MAX_TERMS_PER_QUERY],
    or_group_len: usize,
}

impl RawDenseTableFilters {
    const EMPTY: Self = Self {
        skip: false,
        filter_slices: [RawTickSlice::EMPTY; MAX_TICK_FILTER_SLICES],
        filter_len: 0,
        or_slices: [RawTickSlice::EMPTY; MAX_TICK_FILTER_SLICES],
        or_slice_len: 0,
        or_ranges: [(0, 0); MAX_TERMS_PER_QUERY],
        or_group_len: 0,
    };
}

#[derive(Clone, Copy)]
struct RawArchetypeFilters {
    skip: bool,
    dense_filter_slices: [(usize, RawTickSlice); MAX_TICK_FILTER_SLICES],
    dense_filter_len: usize,
    or_dense_slices: [RawTickSlice; MAX_TICK_FILTER_SLICES],
    or_dense_len: usize,
    or_sparse_clauses: [(ComponentId, bool); MAX_TICK_FILTER_SLICES],
    or_sparse_len: usize,
    or_ranges: [(usize, usize, usize, usize); MAX_TERMS_PER_QUERY],
    or_group_len: usize,
}

impl RawArchetypeFilters {
    const EMPTY: Self = Self {
        skip: false,
        dense_filter_slices: [(0, RawTickSlice::EMPTY); MAX_TICK_FILTER_SLICES],
        dense_filter_len: 0,
        or_dense_slices: [RawTickSlice::EMPTY; MAX_TICK_FILTER_SLICES],
        or_dense_len: 0,
        or_sparse_clauses: [(ComponentId::new(0), false); MAX_TICK_FILTER_SLICES],
        or_sparse_len: 0,
        or_ranges: [(0, 0, 0, 0); MAX_TERMS_PER_QUERY],
        or_group_len: 0,
    };
}

struct FilterScratch {
    dense: RawDenseTableFilters,
    arch: RawArchetypeFilters,
}

// SAFETY: `FilterScratch` stores raw pointers to `Table` tick slices only during an active
// `HostIter` invocation guarded by `cached_filter_cursor`; never read across frames or threads.
unsafe impl Send for FilterScratch {}
// SAFETY: Shared references to `GuestSystem` never read or dereference `FilterScratch`.
unsafe impl Sync for FilterScratch {}

#[inline]
fn get_or_alloc_scratch<'a>(
    sparse_scratch: &'a mut Option<&mut SparseRowScratch>,
    owned_scratch: &'a mut Option<Box<SparseRowScratch>>,
    term_count: usize,
) -> &'a mut SparseRowScratch {
    if let Some(scratch) = sparse_scratch.as_deref_mut() {
        scratch.ensure_terms(term_count);
        scratch
    } else {
        let scratch = owned_scratch.get_or_insert_with(|| Box::new(SparseRowScratch::default()));
        scratch.ensure_terms(term_count);
        scratch
    }
}

pub(crate) struct QueryGroupIterInput<'q> {
    pub(crate) is_dense: bool,
    pub(crate) matched_tables: &'q [TableId],
    pub(crate) matched_archetypes: &'q [ArchetypeId],
    pub(crate) has_masks: &'q [u32],
    pub(crate) plan: &'q QueryPlan,
    pub(crate) sparse_scratch: &'q mut SparseRowScratch,
    pub(crate) msg_cursors: &'q mut [Option<Box<dyn Any + Send + Sync>>],
}

/// Concrete `#[repr(C)]` host chunk and resource iterator backing opaque [`BamIter`].
#[repr(C)]
pub struct HostIter<'w> {
    /// Leading handle validation header (`ITER_MAGIC`) at byte offset `0`.
    pub header: HandleHeader,
    tables: Option<&'w Tables>,
    backing: IterBacking<'w>,
    pub(crate) plan: &'w QueryPlan,
    last_run: Tick,
    this_run: Tick,
    pub(crate) world: Option<UnsafeWorldCell<'w>>,
    sparse_scratch: Option<&'w mut SparseRowScratch>,
    owned_scratch: Option<Box<SparseRowScratch>>,
    pub(crate) msg_cursors: Option<&'w mut [Option<Box<dyn Any + Send + Sync>>]>,
    chunks_yielded: usize,
    entities_yielded: usize,
    current_entities_ptr: *const FfiEntity,
    current_entity_count: usize,
    current_table_len: usize,
    current_start_row_idx: usize,
    has_masks: &'w [u32],
    par_commands: Option<core::ptr::NonNull<HostCommands<'w>>>,
    /// Bitmask tracking which writable terms have already marked ticks in the current chunk.
    marked_changed_mask: u32,
    /// Bitmask tracking which sparse terms have materialized indirect row pointers in the current chunk.
    materialized_sparse_mask: u32,
    /// Bitmask tracking which sparse terms are actually present on the current archetype.
    present_sparse_mask: u32,
    cached_table_cursor: Option<NonMaxU32>,
    cached_filter_cursor: Option<NonMaxU32>,
    pub(crate) in_par_iter: bool,
}

const _: () = assert!(core::mem::offset_of!(HostIter<'_>, header) == 0);
const _: () = assert!(core::mem::size_of::<HostIter<'_>>() <= 256);

impl<'w> HostIter<'w> {
    #[inline]
    fn from_backing(
        tables: &'w Tables,
        backing: IterBacking<'w>,
        plan: &'w QueryPlan,
        ticks: RunTicks,
        world: Option<UnsafeWorldCell<'w>>,
        sparse_scratch: Option<&'w mut SparseRowScratch>,
        in_par_iter: bool,
    ) -> Self {
        Self {
            header: HandleHeader::new(ITER_MAGIC),
            tables: Some(tables),
            backing,
            plan,
            last_run: ticks.last_run,
            this_run: ticks.this_run,
            world,
            sparse_scratch,
            owned_scratch: None,
            msg_cursors: None,
            chunks_yielded: 0,
            entities_yielded: 0,
            current_entities_ptr: core::ptr::null(),
            current_entity_count: 0,
            current_table_len: 0,
            current_start_row_idx: 0,
            has_masks: &[],
            par_commands: None,
            marked_changed_mask: 0,
            materialized_sparse_mask: 0,
            present_sparse_mask: 0,
            cached_table_cursor: None,
            cached_filter_cursor: None,
            in_par_iter,
        }
    }

    #[inline]
    pub(crate) fn from_query_group(
        world: UnsafeWorldCell<'w>,
        storages: (&'w Tables, &'w Archetypes, &'w SparseSets),
        group: QueryGroupIterInput<'w>,
        last_run: Tick,
        this_run: Tick,
        generation: u32,
    ) -> Self {
        let (tables, archetypes, sparse_sets) = storages;
        group.sparse_scratch.ensure_terms(group.plan.terms.len());
        let backing = if !group.plan.has_entity_terms {
            IterBacking::ResourceOnly
        } else if group.is_dense {
            IterBacking::DenseTables {
                matched_tables: group.matched_tables,
                table_cursor: 0,
                row_cursor: 0,
                single_table_end_row: None,
            }
        } else {
            IterBacking::Archetypes {
                archetypes,
                sparse_sets,
                matched_archetypes: group.matched_archetypes,
                archetype_cursor: 0,
                entity_cursor: 0,
                single_arch_end_entity: None,
            }
        };
        Self {
            header: HandleHeader::with_generation(ITER_MAGIC, generation),
            tables: Some(tables),
            backing,
            plan: group.plan,
            last_run,
            this_run,
            world: Some(world),
            sparse_scratch: Some(group.sparse_scratch),
            owned_scratch: None,
            msg_cursors: Some(group.msg_cursors),
            chunks_yielded: 0,
            entities_yielded: 0,
            current_entities_ptr: core::ptr::null(),
            current_entity_count: 0,
            current_table_len: 0,
            current_start_row_idx: 0,
            has_masks: group.has_masks,
            par_commands: None,
            marked_changed_mask: 0,
            materialized_sparse_mask: 0,
            present_sparse_mask: 0,
            cached_table_cursor: None,
            cached_filter_cursor: None,
            in_par_iter: false,
        }
    }

    /// Constructs a dense `Table` chunk iterator.
    pub fn new_dense(
        tables: &'w Tables,
        matched_tables: &'w [TableId],
        plan: &'w QueryPlan,
        last_run: Tick,
        this_run: Tick,
    ) -> Self {
        let backing = if plan.has_entity_terms {
            IterBacking::DenseTables {
                matched_tables,
                table_cursor: 0,
                row_cursor: 0,
                single_table_end_row: None,
            }
        } else {
            IterBacking::ResourceOnly
        };
        Self::from_backing(
            tables,
            backing,
            plan,
            RunTicks { last_run, this_run },
            None,
            None,
            false,
        )
    }

    /// Constructs an archetype-aware iterator supporting indirect `SparseSet` row batching.
    pub fn new_archetypal(
        tables: &'w Tables,
        archetypes: &'w Archetypes,
        sparse_sets: &'w SparseSets,
        matched_archetypes: &'w [ArchetypeId],
        plan: &'w QueryPlan,
        sparse_scratch: &'w mut SparseRowScratch,
        last_run: Tick,
        this_run: Tick,
    ) -> Self {
        sparse_scratch.ensure_terms(plan.terms.len());
        let backing = if plan.has_entity_terms {
            IterBacking::Archetypes {
                archetypes,
                sparse_sets,
                matched_archetypes,
                archetype_cursor: 0,
                entity_cursor: 0,
                single_arch_end_entity: None,
            }
        } else {
            IterBacking::ResourceOnly
        };
        Self::from_backing(
            tables,
            backing,
            plan,
            RunTicks { last_run, this_run },
            None,
            Some(sparse_scratch),
            false,
        )
    }

    /// Overrides the handle generation (for zero-atomic per-frame initialization when `generation != 0`).
    #[inline]
    pub fn with_generation(mut self, generation: u32) -> Self {
        self.header = HandleHeader::with_generation(ITER_MAGIC, generation);
        self
    }

    /// Attaches reusable per-query-group [`SparseRowScratch`] storage (`has_*_buf`, partition
    /// scratch, and message slice staging) for zero-allocation steady-state iteration.
    #[inline]
    pub fn with_scratch(mut self, scratch: &'w mut SparseRowScratch) -> Self {
        scratch.ensure_terms(self.plan.terms.len());
        self.sparse_scratch = Some(scratch);
        self.owned_scratch = None;
        self
    }

    /// Attaches precomputed per-matched-table and per-matched-archetype `Has<T>` bitmasks from
    /// [`crate::system::query_group::CachedQueryGroup`].
    #[inline]
    pub(crate) const fn with_has_masks(
        mut self,
        table_has_masks: &'w [u32],
        arch_has_masks: &'w [u32],
    ) -> Self {
        self.has_masks = match self.backing {
            IterBacking::DenseTables { .. } => table_has_masks,
            IterBacking::Archetypes { .. } => arch_has_masks,
            IterBacking::ResourceOnly => &[],
        };
        self
    }

    /// Mutably attaches the active system [`HostCommands`] pointer in place.
    #[inline]
    pub(crate) fn set_par_commands(&mut self, commands: *mut HostCommands<'w>) {
        self.par_commands = core::ptr::NonNull::new(commands);
    }

    /// Attaches `UnsafeWorldCell` so `res_ptr` and `res_mut_ptr` can resolve singleton resources.
    #[inline]
    pub fn with_world(mut self, world: UnsafeWorldCell<'w>) -> Self {
        self.world = Some(world);
        self
    }

    /// Attaches per-system `MessageCursor` state for `MESSAGE_READ` terms.
    #[inline]
    pub fn with_msg_cursors(
        mut self,
        cursors: &'w mut [Option<Box<dyn Any + Send + Sync>>],
    ) -> Self {
        self.msg_cursors = Some(cursors);
        self
    }

    #[inline]
    fn active_scratch(&self) -> Option<&SparseRowScratch> {
        self.sparse_scratch
            .as_deref()
            .or(self.owned_scratch.as_deref())
    }

    #[inline]
    fn active_scratch_mut_if_present(&mut self) -> Option<&mut SparseRowScratch> {
        if let Some(ref mut scratch) = self.sparse_scratch {
            Some(&mut **scratch)
        } else {
            self.owned_scratch.as_deref_mut()
        }
    }

    #[inline]
    fn active_scratch_mut(&mut self) -> &mut SparseRowScratch {
        get_or_alloc_scratch(
            &mut self.sparse_scratch,
            &mut self.owned_scratch,
            self.plan.terms.len(),
        )
    }

    #[inline]
    fn active_col(&self, term_index: usize) -> Option<RawActiveDenseColumn> {
        self.active_scratch()
            .and_then(|s| s.active_cols.get(term_index).copied().flatten())
    }

    /// Returns `(chunks_yielded, entities_yielded)` accumulated during this iterator's lifetime.
    #[inline]
    pub const fn stats(&self) -> (usize, usize) {
        (self.chunks_yielded, self.entities_yielded)
    }

    /// Casts `&mut self` to an opaque `*mut BamIter` FFI pointer.
    #[inline]
    pub fn as_ffi_ptr(&mut self) -> *mut BamIter {
        (self as *mut Self).cast::<BamIter>()
    }

    #[inline(always)]
    fn row_matches_dense_slices(
        slices: &[RawTickSlice],
        row: usize,
        last_run: Tick,
        this_run: Tick,
    ) -> bool {
        let mut ok = true;
        for &raw_ticks in slices {
            // SAFETY: Caller guarantees `raw_ticks` points to a live `Table` column tick slice of
            // length `table_len > row`.
            let ticks = unsafe { raw_ticks.as_slice() };
            // SAFETY: `row < table_len == ticks.len()`, and shared tick access was declared in `FilteredAccess`.
            let tick = unsafe { *ticks.get_unchecked(row).get() };
            ok &= tick.is_newer_than(last_run, this_run);
        }
        ok
    }

    #[inline(always)]
    fn row_matches_dense_slices_and_or(
        slices: &[RawTickSlice],
        or_slices: &[RawTickSlice],
        or_ranges: &[(usize, usize)],
        row: usize,
        last_run: Tick,
        this_run: Tick,
    ) -> bool {
        if !Self::row_matches_dense_slices(slices, row, last_run, this_run) {
            return false;
        }
        for &(start, end) in or_ranges {
            let mut any_match = false;
            for &raw_ticks in &or_slices[start..end] {
                // SAFETY: Caller guarantees `raw_ticks` points to a live `Table` column tick slice of
                // length `table_len > row`.
                let ticks = unsafe { raw_ticks.as_slice() };
                // SAFETY: `row < table_len == ticks.len()`, and shared tick access was declared in `FilteredAccess`.
                let tick = unsafe { *ticks.get_unchecked(row).get() };
                if tick.is_newer_than(last_run, this_run) {
                    any_match = true;
                    break;
                }
            }
            if !any_match {
                return false;
            }
        }
        true
    }

    #[inline]
    fn entity_matches_archetypal_filters(
        af: &RawArchetypeFilters,
        sparse_sets: &'w SparseSets,
        tick_filters: &[TickFilterTerm],
        entity: Entity,
        table_row_idx: usize,
        table_len: usize,
        ticks: RunTicks,
    ) -> bool {
        if table_row_idx >= table_len {
            return false;
        }
        for &(_, raw_ticks) in &af.dense_filter_slices[..af.dense_filter_len] {
            // SAFETY: Caller guarantees `raw_ticks` points to a live `Table` column tick slice of
            // length `table_len > table_row_idx`.
            let col_ticks = unsafe { raw_ticks.as_slice() };
            // SAFETY: `table_row_idx < table_len == col_ticks.len()`.
            let tick = unsafe { *col_ticks.get_unchecked(table_row_idx).get() };
            if !tick.is_newer_than(ticks.last_run, ticks.this_run) {
                return false;
            }
        }
        for tf in tick_filters {
            if !tf.is_sparse {
                continue;
            }
            let Some(ss) = sparse_sets.get(tf.component_id) else {
                return false;
            };
            let tick_cell = if tf.is_added {
                ss.get_added_tick(entity)
            } else {
                ss.get_changed_tick(entity)
            };
            let Some(cell) = tick_cell else {
                return false;
            };
            // SAFETY: Shared sparse component tick access was declared in `FilteredAccess`.
            let tick = unsafe { *cell.get() };
            if !tick.is_newer_than(ticks.last_run, ticks.this_run) {
                return false;
            }
        }
        true
    }

    #[inline]
    fn entity_matches_archetypal_or_groups(
        af: &RawArchetypeFilters,
        sparse_sets: &'w SparseSets,
        entity: Entity,
        table_row_idx: usize,
        table_len: usize,
        ticks: RunTicks,
    ) -> bool {
        if table_row_idx >= table_len {
            return false;
        }
        let or_dense = &af.or_dense_slices[..af.or_dense_len];
        let or_sparse = &af.or_sparse_clauses[..af.or_sparse_len];
        for &(d_start, d_end, s_start, s_end) in &af.or_ranges[..af.or_group_len] {
            let mut any_match = false;
            for &raw_ticks in &or_dense[d_start..d_end] {
                // SAFETY: Caller guarantees `raw_ticks` points to a live `Table` column tick slice of
                // length `table_len > table_row_idx`.
                let col_ticks = unsafe { raw_ticks.as_slice() };
                // SAFETY: `table_row_idx < table_len == col_ticks.len()`.
                let tick = unsafe { *col_ticks.get_unchecked(table_row_idx).get() };
                if tick.is_newer_than(ticks.last_run, ticks.this_run) {
                    any_match = true;
                    break;
                }
            }
            if !any_match {
                for &(cid, is_added) in &or_sparse[s_start..s_end] {
                    let Some(ss) = sparse_sets.get(cid) else {
                        continue;
                    };
                    let tick_cell = if is_added {
                        ss.get_added_tick(entity)
                    } else {
                        ss.get_changed_tick(entity)
                    };
                    let Some(cell) = tick_cell else {
                        continue;
                    };
                    // SAFETY: Shared sparse component tick access was declared in `FilteredAccess`.
                    let tick = unsafe { *cell.get() };
                    if tick.is_newer_than(ticks.last_run, ticks.this_run) {
                        any_match = true;
                        break;
                    }
                }
            }
            if !any_match {
                return false;
            }
        }
        true
    }

    #[inline]
    fn prepare_dense_table_filters(
        table: &bevy_ecs::storage::Table,
        full_table_len: usize,
        plan: &QueryPlan,
        last_run: Tick,
        this_run: Tick,
        out: &mut RawDenseTableFilters,
    ) {
        let tick_filters = plan.tick_filters.as_slice();
        let or_groups = plan.or_groups.as_slice();
        let mut skip = false;

        for (f_idx, tf) in tick_filters.iter().enumerate() {
            let Some(col) = table.get_column(tf.component_id) else {
                skip = true;
                break;
            };
            if !tf.is_added
                && let Some(summary) = col.get_summary_tick()
                && !summary.get().is_newer_than(last_run, this_run)
            {
                skip = true;
                break;
            }
            let slice = if tf.is_added {
                // SAFETY: `full_table_len == table.entity_count() as usize`.
                unsafe { col.get_added_ticks_slice(full_table_len) }
            } else {
                // SAFETY: `full_table_len == table.entity_count() as usize`.
                unsafe { col.get_changed_ticks_slice(full_table_len) }
            };
            out.filter_slices[f_idx] = RawTickSlice::from_slice(slice);
        }

        let mut or_slice_len = 0usize;
        let mut or_group_len = 0usize;

        if !skip && !or_groups.is_empty() {
            for group in or_groups {
                let mut trivially_satisfied = false;
                let start_slice = or_slice_len;
                for clause in &group.terms {
                    if clause.has_tick_filter() {
                        let Some(col) = table.get_column(clause.component_id) else {
                            continue;
                        };
                        let is_added = clause.has_added_filter();
                        if !is_added
                            && let Some(summary) = col.get_summary_tick()
                            && !summary.get().is_newer_than(last_run, this_run)
                        {
                            continue;
                        }
                        let slice = if is_added {
                            // SAFETY: `full_table_len == table.entity_count() as usize`.
                            unsafe { col.get_added_ticks_slice(full_table_len) }
                        } else {
                            // SAFETY: `full_table_len == table.entity_count() as usize`.
                            unsafe { col.get_changed_ticks_slice(full_table_len) }
                        };
                        out.or_slices[or_slice_len] = RawTickSlice::from_slice(slice);
                        or_slice_len += 1;
                    } else if clause.mode == AccessMode::FILTER_ONLY
                        && table.has_column(clause.component_id)
                        || clause.mode == AccessMode::WITHOUT
                            && !table.has_column(clause.component_id)
                    {
                        trivially_satisfied = true;
                        break;
                    }
                }
                if trivially_satisfied {
                    or_slice_len = start_slice;
                    continue;
                }
                if or_slice_len == start_slice {
                    skip = true;
                    break;
                }
                out.or_ranges[or_group_len] = (start_slice, or_slice_len);
                or_group_len += 1;
            }
        }

        out.skip = skip;
        out.filter_len = tick_filters.len();
        out.or_slice_len = or_slice_len;
        out.or_group_len = or_group_len;
    }

    #[inline]
    fn prepare_archetype_filters(
        archetype: &bevy_ecs::archetype::Archetype,
        table: &bevy_ecs::storage::Table,
        sparse_sets: &SparseSets,
        table_len: usize,
        plan: &QueryPlan,
        last_run: Tick,
        this_run: Tick,
        out: &mut RawArchetypeFilters,
    ) {
        let tick_filters = plan.tick_filters.as_slice();
        let or_groups = plan.or_groups.as_slice();
        let mut dense_filter_len = 0usize;
        let mut skip = false;

        for (f_idx, tf) in tick_filters.iter().enumerate() {
            if tf.is_sparse {
                continue;
            }
            let Some(col) = table.get_column(tf.component_id) else {
                skip = true;
                break;
            };
            if !tf.is_added
                && let Some(summary) = col.get_summary_tick()
                && !summary.get().is_newer_than(last_run, this_run)
            {
                skip = true;
                break;
            }
            let slice = if tf.is_added {
                // SAFETY: `table_len == table.entity_count() as usize`.
                unsafe { col.get_added_ticks_slice(table_len) }
            } else {
                // SAFETY: `table_len == table.entity_count() as usize`.
                unsafe { col.get_changed_ticks_slice(table_len) }
            };
            out.dense_filter_slices[dense_filter_len] = (f_idx, RawTickSlice::from_slice(slice));
            dense_filter_len += 1;
        }

        let mut or_dense_len = 0usize;
        let mut or_sparse_len = 0usize;
        let mut or_group_len = 0usize;

        if !skip && !or_groups.is_empty() {
            for group in or_groups {
                let mut trivially_satisfied = false;
                let d_start = or_dense_len;
                let s_start = or_sparse_len;
                for clause in &group.terms {
                    if clause.has_tick_filter() {
                        if !archetype.contains(clause.component_id) {
                            continue;
                        }
                        let is_added = clause.has_added_filter();
                        if clause.is_sparse {
                            if sparse_sets.get(clause.component_id).is_some() {
                                out.or_sparse_clauses[or_sparse_len] =
                                    (clause.component_id, is_added);
                                or_sparse_len += 1;
                            }
                        } else if let Some(col) = table.get_column(clause.component_id) {
                            if !is_added
                                && let Some(summary) = col.get_summary_tick()
                                && !summary.get().is_newer_than(last_run, this_run)
                            {
                                continue;
                            }
                            let slice = if is_added {
                                // SAFETY: `table_len == table.entity_count() as usize`.
                                unsafe { col.get_added_ticks_slice(table_len) }
                            } else {
                                // SAFETY: `table_len == table.entity_count() as usize`.
                                unsafe { col.get_changed_ticks_slice(table_len) }
                            };
                            out.or_dense_slices[or_dense_len] = RawTickSlice::from_slice(slice);
                            or_dense_len += 1;
                        }
                    } else if clause.mode == AccessMode::FILTER_ONLY
                        && archetype.contains(clause.component_id)
                        || clause.mode == AccessMode::WITHOUT
                            && !archetype.contains(clause.component_id)
                    {
                        trivially_satisfied = true;
                        break;
                    }
                }
                if trivially_satisfied {
                    or_dense_len = d_start;
                    or_sparse_len = s_start;
                    continue;
                }
                if or_dense_len == d_start && or_sparse_len == s_start {
                    skip = true;
                    break;
                }
                out.or_ranges[or_group_len] = (d_start, or_dense_len, s_start, or_sparse_len);
                or_group_len += 1;
            }
        }

        out.skip = skip;
        out.dense_filter_len = dense_filter_len;
        out.or_dense_len = or_dense_len;
        out.or_sparse_len = or_sparse_len;
        out.or_group_len = or_group_len;
    }

    #[inline(never)]
    fn count_matching_dense_filtered(
        table: &bevy_ecs::storage::Table,
        full_len: usize,
        start: usize,
        limit_len: usize,
        plan: &QueryPlan,
        last_run: Tick,
        this_run: Tick,
    ) -> usize {
        let mut df = RawDenseTableFilters::EMPTY;
        Self::prepare_dense_table_filters(table, full_len, plan, last_run, this_run, &mut df);
        if df.skip {
            return 0;
        }
        let active_slices = &df.filter_slices[..df.filter_len];
        let active_or_slices = &df.or_slices[..df.or_slice_len];
        let active_or_ranges = &df.or_ranges[..df.or_group_len];
        let mut matched = 0usize;
        for row in start..limit_len {
            if Self::row_matches_dense_slices_and_or(
                active_slices,
                active_or_slices,
                active_or_ranges,
                row,
                last_run,
                this_run,
            ) {
                matched += 1;
            }
        }
        matched
    }

    #[inline(never)]
    fn count_matching_arch_filtered(
        archetype: &bevy_ecs::archetype::Archetype,
        table: &bevy_ecs::storage::Table,
        sparse_sets: &'w SparseSets,
        range: (usize, usize),
        plan: &QueryPlan,
        ticks: RunTicks,
    ) -> usize {
        let (start, limit_entities) = range;
        let table_len = table.entity_count() as usize;
        let mut af = RawArchetypeFilters::EMPTY;
        Self::prepare_archetype_filters(
            archetype,
            table,
            sparse_sets,
            table_len,
            plan,
            ticks.last_run,
            ticks.this_run,
            &mut af,
        );
        if af.skip {
            return 0;
        }
        let tick_filters = plan.tick_filters.as_slice();
        let has_tick_filters = !tick_filters.is_empty();
        let mut matched = 0usize;
        for cand in &archetype.entities()[start..limit_entities] {
            let row_idx = cand.table_row().index();
            let ok_top = !has_tick_filters
                || Self::entity_matches_archetypal_filters(
                    &af,
                    sparse_sets,
                    tick_filters,
                    cand.id(),
                    row_idx,
                    table_len,
                    ticks,
                );
            let ok_or = af.or_group_len == 0
                || Self::entity_matches_archetypal_or_groups(
                    &af,
                    sparse_sets,
                    cand.id(),
                    row_idx,
                    table_len,
                    ticks,
                );
            if ok_top && ok_or {
                matched += 1;
            }
        }
        matched
    }

    /// Counts matching entities across this iterator without advancing or mutating cursor state.
    pub(crate) fn count_matching(&self) -> usize {
        let Some(tables) = self.tables else {
            return 0;
        };
        let plan = self.plan;
        let last_run = self.last_run;
        let this_run = self.this_run;
        let has_tick_filters = !plan.tick_filters.is_empty();
        let has_or_groups = !plan.or_groups.is_empty();

        match self.backing {
            IterBacking::ResourceOnly => 0,
            IterBacking::DenseTables {
                matched_tables,
                table_cursor,
                row_cursor,
                single_table_end_row,
            } => {
                let mut total = 0usize;
                for (t_idx, &table_id) in matched_tables.iter().enumerate() {
                    let Some(table) = tables.get(table_id) else {
                        continue;
                    };
                    let full_len = table.entity_count() as usize;
                    let limit_len = single_table_end_row
                        .map(|end| end.min(full_len))
                        .unwrap_or(full_len);
                    let start = if self.in_par_iter
                        && single_table_end_row.is_some()
                        && t_idx == table_cursor
                    {
                        row_cursor.min(limit_len)
                    } else {
                        0
                    };
                    if start >= limit_len {
                        continue;
                    }
                    if !has_tick_filters && !has_or_groups {
                        total += limit_len - start;
                        continue;
                    }
                    total += Self::count_matching_dense_filtered(
                        table, full_len, start, limit_len, plan, last_run, this_run,
                    );
                }
                total
            }
            IterBacking::Archetypes {
                archetypes,
                sparse_sets,
                matched_archetypes,
                archetype_cursor,
                entity_cursor,
                single_arch_end_entity,
            } => {
                let ticks = RunTicks { last_run, this_run };
                let mut total = 0usize;
                for (a_idx, &arch_id) in matched_archetypes.iter().enumerate() {
                    let Some(archetype) = archetypes.get(arch_id) else {
                        continue;
                    };
                    let arch_entities = archetype.entities();
                    let limit_entities = single_arch_end_entity
                        .map(|end| end.min(arch_entities.len()))
                        .unwrap_or(arch_entities.len());
                    let start = if self.in_par_iter
                        && single_arch_end_entity.is_some()
                        && a_idx == archetype_cursor
                    {
                        entity_cursor.min(limit_entities)
                    } else {
                        0
                    };
                    if start >= limit_entities {
                        continue;
                    }
                    if !has_tick_filters && !has_or_groups {
                        total += limit_entities - start;
                        continue;
                    }
                    let Some(table) = tables.get(archetype.table_id()) else {
                        continue;
                    };
                    total += Self::count_matching_arch_filtered(
                        archetype,
                        table,
                        sparse_sets,
                        (start, limit_entities),
                        plan,
                        ticks,
                    );
                }
                total
            }
        }
    }

    /// Returns `true` if the query plan declares any mutable term (`WRITE`, `OPTIONAL_WRITE`,
    /// `RESOURCE_WRITE`, or `MESSAGE_WRITE`).
    #[inline]
    pub(crate) fn has_writable_terms(&self) -> bool {
        self.plan.terms.iter().any(|t| t.mode.is_mutable())
    }

    /// Advances to the next non-empty matching chunk, returning `0` when exhausted.
    pub fn next_chunk(&mut self) -> usize {
        let last_run = self.last_run;
        let this_run = self.this_run;
        let plan = self.plan;
        let Some(tables) = self.tables else {
            self.clear_active_chunk();
            return 0;
        };

        match self.backing {
            IterBacking::ResourceOnly => {
                self.clear_active_chunk();
                return 0;
            }
            IterBacking::DenseTables {
                matched_tables,
                ref mut table_cursor,
                ref mut row_cursor,
                single_table_end_row,
            } => {
                let tick_filters = plan.tick_filters.as_slice();
                let or_groups = plan.or_groups.as_slice();
                let has_tick_filters = !tick_filters.is_empty();
                let has_or_groups = !or_groups.is_empty();
                let clamp_has_len = self.sparse_scratch.is_none() && !plan.has_indices.is_empty();

                while *table_cursor < matched_tables.len() {
                    let cur_tc = *table_cursor;
                    let table_id = matched_tables[cur_tc];
                    let Some(table) = tables.get(table_id) else {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        continue;
                    };

                    let full_table_len = table.entity_count() as usize;
                    let limit_len = single_table_end_row
                        .map(|end| end.min(full_table_len))
                        .unwrap_or(full_table_len);
                    if limit_len == 0 || *row_cursor >= limit_len {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        continue;
                    }

                    if !has_tick_filters && !has_or_groups {
                        let start = *row_cursor;
                        let mut run_len = limit_len - start;
                        if clamp_has_len && run_len > 4096 {
                            run_len = 4096;
                        }
                        *row_cursor = start + run_len;
                        if *row_cursor >= limit_len {
                            *table_cursor += 1;
                            *row_cursor = 0;
                        }
                        let pre_has_mask = self.has_masks.get(cur_tc).copied();
                        self.current_entities_ptr =
                            table.entities()[start..].as_ptr().cast::<FfiEntity>();
                        self.current_entity_count = run_len;
                        self.populate_dense_columns_cached(
                            cur_tc,
                            table,
                            full_table_len,
                            start,
                            run_len,
                        );
                        self.populate_has_columns(run_len, pre_has_mask, |cid| {
                            table.has_column(cid)
                        });
                        self.chunks_yielded += 1;
                        self.entities_yielded += run_len;
                        return run_len;
                    }

                    let df = {
                        let scratch = get_or_alloc_scratch(
                            &mut self.sparse_scratch,
                            &mut self.owned_scratch,
                            plan.terms.len(),
                        );
                        let fs = scratch.filter_scratch.get_or_insert_with(|| {
                            Box::new(FilterScratch {
                                dense: RawDenseTableFilters::EMPTY,
                                arch: RawArchetypeFilters::EMPTY,
                            })
                        });
                        let tc_nonmax = u32::try_from(cur_tc).ok().and_then(NonMaxU32::new);
                        if tc_nonmax.is_none() || self.cached_filter_cursor != tc_nonmax {
                            Self::prepare_dense_table_filters(
                                table,
                                full_table_len,
                                plan,
                                last_run,
                                this_run,
                                &mut fs.dense,
                            );
                            self.cached_filter_cursor = tc_nonmax;
                        }
                        &fs.dense
                    };
                    if df.skip {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        continue;
                    }

                    let active_slices = &df.filter_slices[..df.filter_len];
                    let active_or_slices = &df.or_slices[..df.or_slice_len];
                    let active_or_ranges = &df.or_ranges[..df.or_group_len];

                    let (start_row_idx, mut run_len) =
                        if active_slices.is_empty() && active_or_ranges.is_empty() {
                            let start = *row_cursor;
                            (start, limit_len - start)
                        } else {
                            let mut start_row_idx = *row_cursor;
                            while start_row_idx < limit_len
                                && !Self::row_matches_dense_slices_and_or(
                                    active_slices,
                                    active_or_slices,
                                    active_or_ranges,
                                    start_row_idx,
                                    last_run,
                                    this_run,
                                )
                            {
                                start_row_idx += 1;
                            }

                            if start_row_idx >= limit_len {
                                *table_cursor += 1;
                                *row_cursor = 0;
                                continue;
                            }

                            let mut run_len = 1usize;
                            while start_row_idx + run_len < limit_len
                                && Self::row_matches_dense_slices_and_or(
                                    active_slices,
                                    active_or_slices,
                                    active_or_ranges,
                                    start_row_idx + run_len,
                                    last_run,
                                    this_run,
                                )
                            {
                                run_len += 1;
                            }
                            (start_row_idx, run_len)
                        };

                    if clamp_has_len && run_len > 4096 {
                        run_len = 4096;
                    }
                    *row_cursor = start_row_idx + run_len;
                    if *row_cursor >= limit_len {
                        *table_cursor += 1;
                        *row_cursor = 0;
                    }

                    let pre_has_mask = self.has_masks.get(cur_tc).copied();
                    self.current_entities_ptr = table.entities()[start_row_idx..]
                        .as_ptr()
                        .cast::<FfiEntity>();
                    self.current_entity_count = run_len;
                    self.populate_dense_columns_cached(
                        cur_tc,
                        table,
                        full_table_len,
                        start_row_idx,
                        run_len,
                    );
                    self.populate_has_columns(run_len, pre_has_mask, |cid| table.has_column(cid));
                    self.chunks_yielded += 1;
                    self.entities_yielded += run_len;
                    return run_len;
                }
            }
            IterBacking::Archetypes {
                archetypes,
                sparse_sets,
                matched_archetypes,
                ref mut archetype_cursor,
                ref mut entity_cursor,
                single_arch_end_entity,
            } => {
                let tick_filters = plan.tick_filters.as_slice();
                let has_tick_filters = !tick_filters.is_empty();
                let has_or_groups = !plan.or_groups.is_empty();
                let require_contiguous_table_rows = plan.dense_data_mask != 0;
                let clamp_has_len = self.sparse_scratch.is_none() && !plan.has_indices.is_empty();

                while *archetype_cursor < matched_archetypes.len() {
                    let cur_ac = *archetype_cursor;
                    let arch_id = matched_archetypes[cur_ac];
                    let Some(archetype) = archetypes.get(arch_id) else {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    };

                    let arch_entities = archetype.entities();
                    let limit_entities = single_arch_end_entity
                        .map(|end| end.min(arch_entities.len()))
                        .unwrap_or(arch_entities.len());
                    if *entity_cursor >= limit_entities {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    }

                    let Some(table) = tables.get(archetype.table_id()) else {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    };
                    let table_len = table.entity_count() as usize;

                    let af_opt: Option<&RawArchetypeFilters> = if has_tick_filters || has_or_groups
                    {
                        let scratch = get_or_alloc_scratch(
                            &mut self.sparse_scratch,
                            &mut self.owned_scratch,
                            plan.terms.len(),
                        );
                        let fs = scratch.filter_scratch.get_or_insert_with(|| {
                            Box::new(FilterScratch {
                                dense: RawDenseTableFilters::EMPTY,
                                arch: RawArchetypeFilters::EMPTY,
                            })
                        });
                        let ac_nonmax = u32::try_from(cur_ac).ok().and_then(NonMaxU32::new);
                        if ac_nonmax.is_none() || self.cached_filter_cursor != ac_nonmax {
                            Self::prepare_archetype_filters(
                                archetype,
                                table,
                                sparse_sets,
                                table_len,
                                plan,
                                last_run,
                                this_run,
                                &mut fs.arch,
                            );
                            self.cached_filter_cursor = ac_nonmax;
                        }
                        if fs.arch.skip {
                            *archetype_cursor += 1;
                            *entity_cursor = 0;
                            continue;
                        }
                        Some(&fs.arch)
                    } else {
                        None
                    };

                    let ticks = RunTicks { last_run, this_run };
                    let evaluate_row_filters =
                        af_opt.is_some_and(|af| has_tick_filters || af.or_group_len != 0);

                    if evaluate_row_filters && let Some(af) = af_opt {
                        while *entity_cursor < limit_entities {
                            let cand = &arch_entities[*entity_cursor];
                            let row_idx = cand.table_row().index();
                            let ok_top = !has_tick_filters
                                || Self::entity_matches_archetypal_filters(
                                    af,
                                    sparse_sets,
                                    tick_filters,
                                    cand.id(),
                                    row_idx,
                                    table_len,
                                    ticks,
                                );
                            let ok_or = af.or_group_len == 0
                                || Self::entity_matches_archetypal_or_groups(
                                    af,
                                    sparse_sets,
                                    cand.id(),
                                    row_idx,
                                    table_len,
                                    ticks,
                                );
                            if ok_top && ok_or {
                                break;
                            }
                            *entity_cursor += 1;
                        }
                        if *entity_cursor >= limit_entities {
                            *archetype_cursor += 1;
                            *entity_cursor = 0;
                            continue;
                        }
                    }

                    let start_entity_idx = *entity_cursor;
                    let first = &arch_entities[start_entity_idx];
                    let start_row_idx = first.table_row().index();
                    let mut contiguous_table_rows = true;

                    // Batch multi-entity runs even when `sparse_data_mask != 0`!
                    let mut run_len = 1usize;
                    let max_run_end = if clamp_has_len {
                        (start_entity_idx + 4096).min(limit_entities)
                    } else {
                        limit_entities
                    };
                    while start_entity_idx + run_len < max_run_end {
                        let next_ent = &arch_entities[start_entity_idx + run_len];
                        let next_row_idx = next_ent.table_row().index();
                        if next_row_idx != start_row_idx + run_len {
                            if require_contiguous_table_rows {
                                break;
                            }
                            contiguous_table_rows = false;
                        }
                        if evaluate_row_filters && let Some(af) = af_opt {
                            let ok_top = !has_tick_filters
                                || Self::entity_matches_archetypal_filters(
                                    af,
                                    sparse_sets,
                                    tick_filters,
                                    next_ent.id(),
                                    next_row_idx,
                                    table_len,
                                    ticks,
                                );
                            let ok_or = af.or_group_len == 0
                                || Self::entity_matches_archetypal_or_groups(
                                    af,
                                    sparse_sets,
                                    next_ent.id(),
                                    next_row_idx,
                                    table_len,
                                    ticks,
                                );
                            if !(ok_top && ok_or) {
                                break;
                            }
                        }
                        run_len += 1;
                    }

                    *entity_cursor = start_entity_idx + run_len;
                    if *entity_cursor >= limit_entities {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                    }

                    let pre_has_mask = self.has_masks.get(cur_ac).copied();
                    self.clear_active_chunk();
                    self.current_entity_count = run_len;
                    self.chunks_yielded += 1;
                    self.entities_yielded += run_len;

                    if contiguous_table_rows && start_row_idx + run_len <= table_len {
                        self.current_entities_ptr = table.entities()[start_row_idx..]
                            .as_ptr()
                            .cast::<FfiEntity>();
                    } else {
                        let scratch = self.active_scratch_mut();
                        scratch.entities.clear();
                        scratch.entities.reserve(run_len);
                        for arch_ent in &arch_entities[start_entity_idx..start_entity_idx + run_len]
                        {
                            scratch
                                .entities
                                .push(FfiEntity::from_bits(arch_ent.id().to_bits()));
                        }
                        self.current_entities_ptr = scratch.entities.as_ptr();
                    }

                    if plan.dense_data_mask != 0 {
                        self.populate_dense_columns_inner(table, table_len, start_row_idx, run_len);
                    }
                    self.populate_has_columns(run_len, pre_has_mask, |cid| archetype.contains(cid));

                    // Eagerly materialize sparse row pointers for declared sparse data columns into
                    // reused scratch buffers so `iter_column_rows_readonly`/`mutable` are O(1).
                    if plan.sparse_data_mask != 0 {
                        let scratch = get_or_alloc_scratch(
                            &mut self.sparse_scratch,
                            &mut self.owned_scratch,
                            plan.terms.len(),
                        );
                        let mut mask = plan.sparse_data_mask;
                        while mask != 0 {
                            let idx = mask.trailing_zeros() as usize;
                            mask &= mask - 1;
                            let term = &plan.terms[idx];
                            if !archetype.contains(term.component_id) {
                                continue;
                            }
                            let Some(ss) = sparse_sets.get(term.component_id) else {
                                continue;
                            };
                            let rows_vec = &mut scratch.row_ptrs[idx];
                            let ticks_vec = &mut scratch.tick_ptrs[idx];
                            rows_vec.clear();
                            ticks_vec.clear();
                            rows_vec.reserve(run_len);
                            ticks_vec.reserve(run_len);
                            let mut all_present = true;

                            for arch_ent in
                                &arch_entities[start_entity_idx..start_entity_idx + run_len]
                            {
                                if let Some((ptr, tick_cells)) = ss.get_with_ticks(arch_ent.id()) {
                                    rows_vec.push(ptr.as_ptr().cast_mut());
                                    ticks_vec.push(tick_cells.changed as *const UnsafeCell<Tick>);
                                } else {
                                    all_present = false;
                                    break;
                                }
                            }

                            if all_present {
                                self.present_sparse_mask |= 1u32 << idx;
                                self.materialized_sparse_mask |= 1u32 << idx;
                            }
                        }
                    }

                    return run_len;
                }
            }
        }

        self.clear_active_chunk();
        0
    }

    /// Resets this iterator back to the first matched chunk (`iter_reset`).
    #[inline]
    pub fn reset(&mut self) {
        match &mut self.backing {
            IterBacking::ResourceOnly => {}
            IterBacking::DenseTables {
                table_cursor,
                row_cursor,
                ..
            } => {
                *table_cursor = 0;
                *row_cursor = 0;
            }
            IterBacking::Archetypes {
                archetype_cursor,
                entity_cursor,
                ..
            } => {
                *archetype_cursor = 0;
                *entity_cursor = 0;
            }
        }
        self.clear_active_chunk();
    }

    /// Clears only the active prefix `0..term_count` and resets per-chunk bitmasks.
    #[inline]
    fn clear_active_chunk(&mut self) {
        self.current_entities_ptr = core::ptr::null();
        self.current_entity_count = 0;
        self.current_table_len = 0;
        self.current_start_row_idx = 0;
        self.marked_changed_mask = 0;
        self.materialized_sparse_mask = 0;
        self.present_sparse_mask = 0;
        let active_len = self.plan.terms.len().min(MAX_TERMS_PER_QUERY);
        if let Some(scratch) = self.active_scratch_mut_if_present() {
            scratch.active_cols[..active_len].fill(None);
        }
    }

    #[inline]
    fn populate_dense_columns_cached(
        &mut self,
        table_cursor: usize,
        table: &'w Table,
        table_len: usize,
        start_row_idx: usize,
        run_len: usize,
    ) {
        self.marked_changed_mask = 0;
        self.materialized_sparse_mask = 0;
        self.present_sparse_mask = 0;
        self.current_table_len = table_len;
        self.current_start_row_idx = start_row_idx;
        let active_len = self.plan.terms.len().min(MAX_TERMS_PER_QUERY);
        if self.plan.dense_data_indices.is_empty() {
            if let Some(scratch) = self.active_scratch_mut_if_present() {
                scratch.active_cols[..active_len].fill(None);
            }
            return;
        }
        let plan = self.plan;
        let tc_nonmax = u32::try_from(table_cursor).ok().and_then(NonMaxU32::new);
        let need_refresh = tc_nonmax.is_none() || self.cached_table_cursor != tc_nonmax;
        let scratch = get_or_alloc_scratch(
            &mut self.sparse_scratch,
            &mut self.owned_scratch,
            plan.terms.len(),
        );
        scratch.active_cols[..active_len].fill(None);
        if need_refresh {
            for &raw_idx in &plan.dense_data_indices {
                let idx = raw_idx as usize;
                scratch.cached_table_cols[idx] = table
                    .get_column(plan.terms[idx].component_id)
                    .map(core::ptr::NonNull::from);
            }
            self.cached_table_cursor = tc_nonmax;
        }
        let Some(start_nonmax) = NonMaxU32::new(start_row_idx as u32) else {
            return;
        };
        let start_row = TableRow::new(start_nonmax);
        for &raw_idx in &plan.dense_data_indices {
            let idx = raw_idx as usize;
            let term = &plan.terms[idx];
            if let Some(col_nn) = scratch.cached_table_cols[idx] {
                // SAFETY: `col_nn` was derived from `&'w Column` on `table` during `'w`.
                let col = unsafe { col_nn.as_ref() };
                // SAFETY: `start_row_idx + run_len <= table_len == table.entity_count() as usize`.
                let data_ptr = unsafe { col.get_data_unchecked(start_row).as_ptr().cast_mut() };
                let Some(data_nn) = core::ptr::NonNull::new(data_ptr) else {
                    continue;
                };
                let (changed_ticks_ptr, summary_tick) = if term.mode.is_mutable() {
                    // SAFETY: `table_len` is `table.entity_count() as usize`.
                    let all_ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                    (
                        all_ticks[start_row_idx..start_row_idx + run_len].as_ptr(),
                        col.get_summary_tick().map(core::ptr::NonNull::from),
                    )
                } else {
                    (core::ptr::null(), None)
                };
                scratch.active_cols[idx] = Some(RawActiveDenseColumn {
                    data_ptr: data_nn,
                    col: Some(col_nn),
                    changed_ticks_ptr,
                    summary_tick,
                });
            }
        }
    }

    #[inline]
    fn populate_dense_columns_inner(
        &mut self,
        table: &'w Table,
        table_len: usize,
        start_row_idx: usize,
        run_len: usize,
    ) {
        self.current_table_len = table_len;
        self.current_start_row_idx = start_row_idx;
        let Some(start_nonmax) = NonMaxU32::new(start_row_idx as u32) else {
            return;
        };
        let start_row = TableRow::new(start_nonmax);
        let plan = self.plan;
        let scratch = get_or_alloc_scratch(
            &mut self.sparse_scratch,
            &mut self.owned_scratch,
            plan.terms.len(),
        );

        // Iterate ONLY precomputed dense data-column indices!
        for &raw_idx in &plan.dense_data_indices {
            let idx = raw_idx as usize;
            let term = &plan.terms[idx];
            if let Some(col) = table.get_column(term.component_id) {
                let col_nn = core::ptr::NonNull::from(col);
                // SAFETY: `start_row_idx + run_len <= table_len == table.entity_count() as usize`.
                let data_ptr = unsafe { col.get_data_unchecked(start_row).as_ptr().cast_mut() };
                let Some(data_nn) = core::ptr::NonNull::new(data_ptr) else {
                    continue;
                };
                let (changed_ticks_ptr, summary_tick) = if term.mode.is_mutable() {
                    // SAFETY: `table_len` is `table.entity_count() as usize`.
                    let all_ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                    (
                        all_ticks[start_row_idx..start_row_idx + run_len].as_ptr(),
                        col.get_summary_tick().map(core::ptr::NonNull::from),
                    )
                } else {
                    (core::ptr::null(), None)
                };

                scratch.active_cols[idx] = Some(RawActiveDenseColumn {
                    data_ptr: data_nn,
                    col: Some(col_nn),
                    changed_ticks_ptr,
                    summary_tick,
                });
            }
        }
    }

    #[inline]
    fn populate_has_columns(
        &mut self,
        run_len: usize,
        precomputed_mask: Option<u32>,
        mut is_present: impl FnMut(ComponentId) -> bool,
    ) {
        const STATIC_HAS_LEN: usize = 4096;
        static STATIC_HAS_TRUE: [FfiBool; STATIC_HAS_LEN] = [FfiBool::TRUE; STATIC_HAS_LEN];
        static STATIC_HAS_FALSE: [FfiBool; STATIC_HAS_LEN] = [FfiBool::FALSE; STATIC_HAS_LEN];

        if self.plan.has_indices.is_empty() || run_len == 0 {
            return;
        }
        let presence_mask = if let Some(mask) = precomputed_mask {
            mask
        } else {
            let mut m = 0u32;
            for &raw_idx in &self.plan.has_indices {
                let idx = raw_idx as usize;
                let term = &self.plan.terms[idx];
                if is_present(term.component_id) {
                    m |= 1u32 << idx;
                }
            }
            m
        };
        let mut need_true = false;
        let mut need_false = false;
        for &raw_idx in &self.plan.has_indices {
            if (presence_mask & (1u32 << (raw_idx as usize))) != 0 {
                need_true = true;
            } else {
                need_false = true;
            }
        }
        let true_ptr = if need_true {
            if run_len <= STATIC_HAS_LEN {
                STATIC_HAS_TRUE.as_ptr().cast_mut().cast::<u8>()
            } else {
                let scratch = self.active_scratch_mut();
                if scratch.has_true_buf.len() < run_len {
                    scratch.has_true_buf.resize(run_len, FfiBool::TRUE);
                }
                scratch.has_true_buf.as_mut_ptr().cast::<u8>()
            }
        } else {
            core::ptr::null_mut()
        };
        let false_ptr = if need_false {
            if run_len <= STATIC_HAS_LEN {
                STATIC_HAS_FALSE.as_ptr().cast_mut().cast::<u8>()
            } else {
                let scratch = self.active_scratch_mut();
                if scratch.has_false_buf.len() < run_len {
                    scratch.has_false_buf.resize(run_len, FfiBool::FALSE);
                }
                scratch.has_false_buf.as_mut_ptr().cast::<u8>()
            }
        } else {
            core::ptr::null_mut()
        };
        let plan = self.plan;
        let scratch = get_or_alloc_scratch(
            &mut self.sparse_scratch,
            &mut self.owned_scratch,
            plan.terms.len(),
        );
        for &raw_idx in &plan.has_indices {
            let idx = raw_idx as usize;
            let data_ptr = if (presence_mask & (1u32 << idx)) != 0 {
                true_ptr
            } else {
                false_ptr
            };
            scratch.active_cols[idx] =
                core::ptr::NonNull::new(data_ptr).map(|data_nn| RawActiveDenseColumn {
                    data_ptr: data_nn,
                    col: None,
                    changed_ticks_ptr: core::ptr::null(),
                    summary_tick: None,
                });
        }
    }
}

impl Drop for HostIter<'_> {
    #[inline]
    fn drop(&mut self) {
        self.header.poison();
    }
}

/// Advances `iter` to the next non-empty chunk, writing chunk length into `*out_len` and returning
/// [`FfiBool::TRUE`] while chunks remain or [`FfiBool::FALSE`] when exhausted.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_len` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_iter_next_chunk(iter: *mut BamIter, out_len: *mut usize) -> FfiBool {
    if out_len.is_null() || !out_len.is_aligned() {
        return FfiBool::FALSE;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_mut(iter) }) else {
        return FfiBool::FALSE;
    };

    let len = host_iter.next_chunk();
    // SAFETY: `out_len` was checked non-null above and caller guarantees writable `usize`.
    unsafe {
        *out_len = len;
    }
    FfiBool::from(len > 0)
}

/// Advances `iter` to the next non-empty chunk and writes `*out_len`, `*out_entities` (when
/// `out_entities` is non-null), and `out_col_ptrs[0..col_count]` in a single FFI call.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_len` must point to writable `usize` storage, or null.
/// - If `out_entities` is non-null, it must point to writable `*const FfiEntity` storage.
/// - If `col_count > 0`, `out_col_ptrs` must point to `col_count` writable `*mut u8` slots.
pub unsafe extern "C" fn host_iter_next_chunk_batch(
    iter: *mut BamIter,
    out_len: *mut usize,
    out_entities: *mut *const FfiEntity,
    out_col_ptrs: *mut *mut u8,
    col_count: usize,
    mark_changed_mask: u32,
) -> FfiBool {
    if out_len.is_null()
        || !out_len.is_aligned()
        || (!out_entities.is_null() && !out_entities.is_aligned())
        || (col_count > 0 && (out_col_ptrs.is_null() || !out_col_ptrs.is_aligned()))
    {
        return FfiBool::FALSE;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_mut(iter) }) else {
        return FfiBool::FALSE;
    };

    let len = host_iter.next_chunk();
    // SAFETY: `out_len` was checked non-null above and caller guarantees writable `usize`.
    unsafe {
        *out_len = len;
    }
    if len == 0 {
        return FfiBool::FALSE;
    }

    if !out_entities.is_null() {
        // SAFETY: `out_entities` was checked non-null above and caller guarantees writable `*const FfiEntity`.
        unsafe {
            *out_entities = host_iter.current_entities_ptr;
        }
    }

    let fill_count = col_count
        .min(host_iter.plan.terms.len())
        .min(MAX_TERMS_PER_QUERY);
    let this_run = host_iter.this_run;

    for term_index in 0..fill_count {
        let term = &host_iter.plan.terms[term_index];
        let bit = 1u32 << term_index;
        let ptr = if !term.is_resource()
            && !term.is_message()
            && (host_iter.plan.or_term_mask & bit) == 0
        {
            if (term.mode == AccessMode::WRITE || term.mode == AccessMode::OPTIONAL_WRITE)
                && !term.is_sparse_set()
                && !term.is_immutable()
            {
                if let Some(col) = host_iter.active_col(term_index) {
                    if (mark_changed_mask & bit) != 0 && (host_iter.marked_changed_mask & bit) == 0
                    {
                        if !col.changed_ticks_ptr.is_null() {
                            // SAFETY: `col.changed_ticks_ptr` points to `len` `UnsafeCell<Tick>`
                            // entries for this active chunk under exclusive component access.
                            let changed_ticks =
                                unsafe { core::slice::from_raw_parts(col.changed_ticks_ptr, len) };
                            for tick_cell in changed_ticks {
                                // SAFETY: Exclusive mutable access to this component's rows is held by this system.
                                unsafe {
                                    *tick_cell.get() = this_run;
                                }
                            }
                        }
                        if let Some(summary) = col.summary_tick {
                            // SAFETY: `summary` points to this column's live `AtomicTick` for `'w`.
                            unsafe { summary.as_ref() }.set(this_run);
                        }
                        host_iter.marked_changed_mask |= bit;
                    }
                    col.data_ptr.as_ptr()
                } else {
                    core::ptr::null_mut()
                }
            } else if (term.mode.is_has() || term.mode.has_entity_column_data())
                && !term.is_sparse_set()
            {
                host_iter
                    .active_col(term_index)
                    .map_or(core::ptr::null_mut(), |col| col.data_ptr.as_ptr())
            } else {
                core::ptr::null_mut()
            }
        } else {
            core::ptr::null_mut()
        };

        // SAFETY: `term_index < fill_count <= col_count`, and caller guarantees `col_count` writable slots.
        unsafe {
            *out_col_ptrs.add(term_index) = ptr;
        }
    }

    for term_index in fill_count..col_count {
        // SAFETY: `term_index < col_count`, and caller guarantees `col_count` writable slots.
        unsafe {
            *out_col_ptrs.add(term_index) = core::ptr::null_mut();
        }
    }

    FfiBool::TRUE
}

/// Retrieves per-row `added` and `changed` tick array pointers (`[u32; chunk_len]`) alongside the
/// enclosing system's `(last_run, this_run)` tick window for dense column `term_index` in the
/// current chunk.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - Any non-null out-pointer (`out_added`, `out_changed`, `out_last_run`, `out_this_run`) must
///   point to aligned writable storage of its respective type.
pub unsafe extern "C" fn host_iter_column_ticks(
    iter: *const BamIter,
    term_index: usize,
    out_added: *mut *const u32,
    out_changed: *mut *const u32,
    out_last_run: *mut u32,
    out_this_run: *mut u32,
) -> FfiResult {
    if (!out_added.is_null() && !out_added.is_aligned())
        || (!out_changed.is_null() && !out_changed.is_aligned())
        || (!out_last_run.is_null() && !out_last_run.is_aligned())
        || (!out_this_run.is_null() && !out_this_run.is_aligned())
    {
        return FfiResult::INVALID_ARGUMENT;
    }

    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_ref(iter) });
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    if host_iter.current_entity_count == 0 {
        return FfiResult::INVALID_ARGUMENT;
    }

    let term = &host_iter.plan.terms[term_index];
    if term.is_resource()
        || term.is_message()
        || (host_iter.plan.or_term_mask & (1u32 << term_index)) != 0
        || !term.mode.has_entity_column_data()
    {
        return FfiResult::INVALID_COMPONENT;
    }
    if term.is_sparse_set() {
        return FfiResult::UNSUPPORTED;
    }

    let Some(col) = host_iter.active_col(term_index) else {
        return FfiResult::NOT_FOUND;
    };
    let table_len = host_iter.current_table_len;
    let start_row_idx = host_iter.current_start_row_idx;
    let run_len = host_iter.current_entity_count;

    if !out_added.is_null() {
        let added_ptr = if let Some(inner_col_nn) = col.col {
            // SAFETY: `inner_col_nn` was captured from `&'w Column` for the active chunk.
            let inner_col = unsafe { inner_col_nn.as_ref() };
            // SAFETY: `table_len == table.entity_count() as usize` and `start_row_idx + run_len <= table_len`.
            let all_added = unsafe { inner_col.get_added_ticks_slice(table_len) };
            let slice = &all_added[start_row_idx..start_row_idx + run_len];
            if slice.is_empty() {
                core::ptr::null()
            } else {
                slice.as_ptr().cast::<u32>()
            }
        } else {
            core::ptr::null()
        };
        // SAFETY: `out_added` is non-null, aligned, and caller guarantees writable storage.
        unsafe {
            *out_added = added_ptr;
        }
    }
    if !out_changed.is_null() {
        let changed_ptr = if !col.changed_ticks_ptr.is_null() {
            col.changed_ticks_ptr.cast::<u32>()
        } else if let Some(inner_col_nn) = col.col {
            // SAFETY: `inner_col_nn` was captured from `&'w Column` for the active chunk.
            let inner_col = unsafe { inner_col_nn.as_ref() };
            // SAFETY: `table_len == table.entity_count() as usize` and `start_row_idx + run_len <= table_len`.
            let all_changed = unsafe { inner_col.get_changed_ticks_slice(table_len) };
            let slice = &all_changed[start_row_idx..start_row_idx + run_len];
            if slice.is_empty() {
                core::ptr::null()
            } else {
                slice.as_ptr().cast::<u32>()
            }
        } else {
            core::ptr::null()
        };
        // SAFETY: `out_changed` is non-null, aligned, and caller guarantees writable storage.
        unsafe {
            *out_changed = changed_ptr;
        }
    }
    if !out_last_run.is_null() {
        // SAFETY: `out_last_run` is non-null, aligned, and caller guarantees writable storage.
        unsafe {
            *out_last_run = host_iter.last_run.get();
        }
    }
    if !out_this_run.is_null() {
        // SAFETY: `out_this_run` is non-null, aligned, and caller guarantees writable storage.
        unsafe {
            *out_this_run = host_iter.this_run.get();
        }
    }

    FfiResult::OK
}

/// Resets `iter` back to its initial chunk (`FfiResult::OK`).
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_reset(iter: *mut BamIter) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    host_iter.reset();
    FfiResult::OK
}

/// Returns a pointer to `[FfiEntity; chunk_len]` for the current chunk, or null.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_entities(iter: *const BamIter) -> *const FfiEntity {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_ref(iter) }) else {
        return core::ptr::null();
    };
    if host_iter.current_entity_count == 0 {
        return core::ptr::null();
    }
    host_iter.current_entities_ptr
}

/// Returns a read-only byte pointer to dense `Table` column `term_index` in the current chunk,
/// or null if out of bounds, absent on an optional term, sparse-set stored, or filter/resource.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_readonly(
    iter: *const BamIter,
    term_index: usize,
) -> *const u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_ref(iter) }) else {
        return core::ptr::null();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &host_iter.plan.terms[term_index];
    if term.is_resource()
        || term.is_message()
        || (host_iter.plan.or_term_mask & (1u32 << term_index)) != 0
    {
        return core::ptr::null();
    }
    if !term.mode.is_has() && (!term.mode.has_entity_column_data() || term.is_sparse_set()) {
        return core::ptr::null();
    }
    host_iter
        .active_col(term_index)
        .map_or(core::ptr::null(), |col| col.data_ptr.as_ptr().cast_const())
}

/// Returns a mutable byte pointer to dense `Table` column `term_index`, marking chunk change ticks
/// at most once per chunk.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_mutable(
    iter: *mut BamIter,
    term_index: usize,
) -> *mut u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_mut(iter) }) else {
        return core::ptr::null_mut();
    };
    let chunk_len = host_iter.current_entity_count;
    if chunk_len == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null_mut();
    }
    let term = &host_iter.plan.terms[term_index];
    if (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_sparse_set()
        || term.is_immutable()
    {
        return core::ptr::null_mut();
    }

    let Some(col) = host_iter.active_col(term_index) else {
        return core::ptr::null_mut();
    };

    let bit = 1u32 << term_index;
    if (host_iter.marked_changed_mask & bit) == 0 {
        let this_run = host_iter.this_run;
        if !col.changed_ticks_ptr.is_null() {
            // SAFETY: `col.changed_ticks_ptr` points to `chunk_len` `UnsafeCell<Tick>` entries for
            // this active chunk under exclusive component access.
            let changed_ticks =
                unsafe { core::slice::from_raw_parts(col.changed_ticks_ptr, chunk_len) };
            for tick_cell in changed_ticks {
                // SAFETY: Exclusive mutable access to this component's rows is held by this system.
                unsafe {
                    *tick_cell.get() = this_run;
                }
            }
        }
        if let Some(summary) = col.summary_tick {
            // SAFETY: `summary` points to this column's live `AtomicTick` for `'w`.
            unsafe { summary.as_ref() }.set(this_run);
        }
        host_iter.marked_changed_mask |= bit;
    }

    col.data_ptr.as_ptr()
}

/// Returns a mutable byte pointer to dense `Table` column `term_index` without marking change ticks.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_mutable_untracked(
    iter: *mut BamIter,
    term_index: usize,
) -> *mut u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_mut(iter) }) else {
        return core::ptr::null_mut();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null_mut();
    }
    let term = &host_iter.plan.terms[term_index];
    if (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_sparse_set()
        || term.is_immutable()
    {
        return core::ptr::null_mut();
    }
    host_iter
        .active_col(term_index)
        .map_or(core::ptr::null_mut(), |col| col.data_ptr.as_ptr())
}

/// Marks rows `start_row..start_row + row_count` in writable column `term_index` as changed.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_mark_changed_range(
    iter: *mut BamIter,
    term_index: usize,
    start_row: usize,
    row_count: usize,
) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });

    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_immutable()
    {
        return FfiResult::PERMISSION_DENIED;
    }

    let Some(end_row) = start_row.checked_add(row_count) else {
        return FfiResult::OUT_OF_RANGE;
    };
    let chunk_len = host_iter.current_entity_count;
    if end_row > chunk_len {
        return FfiResult::OUT_OF_RANGE;
    }
    if row_count == 0 {
        return FfiResult::OK;
    }

    let this_run = host_iter.this_run;
    if term.is_sparse_set() {
        if (host_iter.present_sparse_mask & (1u32 << term_index)) == 0 {
            return if term.mode == AccessMode::OPTIONAL_WRITE {
                FfiResult::OK
            } else {
                FfiResult::INVALID_COMPONENT
            };
        }
        if let Some(scratch) = host_iter.active_scratch() {
            for &tick_cell_ptr in &scratch.tick_ptrs[term_index][start_row..end_row] {
                // SAFETY: `tick_cell_ptr` points to a live `UnsafeCell<Tick>` for this chunk's entity
                // under exclusive `WRITE` / `OPTIONAL_WRITE` component access.
                unsafe {
                    *(*tick_cell_ptr).get() = this_run;
                }
            }
            return FfiResult::OK;
        }
        return FfiResult::INTERNAL_ERROR;
    }

    if let Some(col) = host_iter.active_col(term_index) {
        if !col.changed_ticks_ptr.is_null() {
            // SAFETY: `col.changed_ticks_ptr` points to `chunk_len >= end_row` `UnsafeCell<Tick>`
            // entries for this active chunk under exclusive component access.
            let changed_ticks =
                unsafe { core::slice::from_raw_parts(col.changed_ticks_ptr, chunk_len) };
            for tick_cell in &changed_ticks[start_row..end_row] {
                // SAFETY: `start_row..end_row` is in bounds and `WRITE`/`OPTIONAL_WRITE` access is held.
                unsafe {
                    *tick_cell.get() = this_run;
                }
            }
        }
        if let Some(summary) = col.summary_tick {
            // SAFETY: `summary` points to this column's live `AtomicTick` for `'w`.
            unsafe { summary.as_ref() }.set(this_run);
        }
        FfiResult::OK
    } else if term.mode == AccessMode::OPTIONAL_WRITE {
        FfiResult::OK
    } else {
        FfiResult::INVALID_COMPONENT
    }
}

/// Marks all [`BamRowRange`] spans in `ranges_ptr[0..range_count]` as changed on `term_index`
/// (ABI `0.4` slot `78`).
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - When `range_count > 0`, `ranges_ptr` must point to `range_count` readable [`BamRowRange`] entries.
pub unsafe extern "C" fn host_iter_mark_changed_ranges(
    iter: *mut BamIter,
    term_index: usize,
    ranges_ptr: *const BamRowRange,
    range_count: usize,
) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });

    if range_count > 0 && ranges_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_immutable()
    {
        return FfiResult::PERMISSION_DENIED;
    }
    if range_count == 0 {
        return FfiResult::OK;
    }
    if !ranges_ptr.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }

    // SAFETY: `ranges_ptr` is non-null, aligned, and readable for `range_count` `BamRowRange` entries.
    let ranges = unsafe { core::slice::from_raw_parts(ranges_ptr, range_count) };
    let chunk_len = host_iter.current_entity_count;
    for r in ranges {
        let Some(end_row) = r.end_row() else {
            return FfiResult::OUT_OF_RANGE;
        };
        if end_row > chunk_len {
            return FfiResult::OUT_OF_RANGE;
        }
    }

    let this_run = host_iter.this_run;
    if term.is_sparse_set() {
        if (host_iter.present_sparse_mask & (1u32 << term_index)) == 0 {
            return if term.mode == AccessMode::OPTIONAL_WRITE {
                FfiResult::OK
            } else {
                FfiResult::INVALID_COMPONENT
            };
        }
        let Some(scratch) = host_iter.active_scratch() else {
            return FfiResult::INTERNAL_ERROR;
        };
        for r in ranges {
            if r.row_count == 0 {
                continue;
            }
            let end_row = r.start_row + r.row_count;
            for &tick_cell_ptr in &scratch.tick_ptrs[term_index][r.start_row..end_row] {
                // SAFETY: Exclusive mutable access to this sparse component is held by this system.
                unsafe {
                    *(*tick_cell_ptr).get() = this_run;
                }
            }
        }
        return FfiResult::OK;
    }

    if let Some(col) = host_iter.active_col(term_index) {
        let mut any_marked = false;
        if !col.changed_ticks_ptr.is_null() {
            // SAFETY: `col.changed_ticks_ptr` points to `chunk_len` `UnsafeCell<Tick>` entries for
            // this active chunk under exclusive component access.
            let changed_ticks =
                unsafe { core::slice::from_raw_parts(col.changed_ticks_ptr, chunk_len) };
            for r in ranges {
                if r.row_count == 0 {
                    continue;
                }
                any_marked = true;
                let end_row = r.start_row + r.row_count;
                for tick_cell in &changed_ticks[r.start_row..end_row] {
                    // SAFETY: `r.start_row..end_row` was bounds-checked above and write access is held.
                    unsafe {
                        *tick_cell.get() = this_run;
                    }
                }
            }
        }
        if any_marked && let Some(summary) = col.summary_tick {
            // SAFETY: `summary` points to this column's live `AtomicTick` for `'w`.
            unsafe { summary.as_ref() }.set(this_run);
        }
        FfiResult::OK
    } else if term.mode == AccessMode::OPTIONAL_WRITE {
        FfiResult::OK
    } else {
        FfiResult::INVALID_COMPONENT
    }
}

/// Returns `[ *const u8; chunk_len ]` of indirect per-row pointers for `StorageType::SparseSet`
/// columns, or null if out of bounds, absent on an optional term, or not sparse-set stored.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_rows_readonly(
    iter: *mut BamIter,
    term_index: usize,
) -> *const *const u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_mut(iter) }) else {
        return core::ptr::null();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.is_sparse_set() || !term.mode.has_entity_column_data() {
        return core::ptr::null();
    }
    if (host_iter.present_sparse_mask & (1u32 << term_index)) == 0 {
        return core::ptr::null();
    }
    let Some(scratch) = host_iter.active_scratch() else {
        return core::ptr::null();
    };
    scratch.row_ptrs[term_index].as_ptr().cast::<*const u8>()
}

/// Returns `[ *mut u8; chunk_len ]` of indirect per-row mutable pointers for `StorageType::SparseSet`
/// columns, marking matched rows changed at most once per chunk.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_rows_mutable(
    iter: *mut BamIter,
    term_index: usize,
) -> *const *mut u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_mut(iter) }) else {
        return core::ptr::null();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.is_sparse_set()
        || (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_immutable()
    {
        return core::ptr::null();
    }
    let bit = 1u32 << term_index;
    if (host_iter.present_sparse_mask & bit) == 0 {
        return core::ptr::null();
    }

    let this_run = host_iter.this_run;
    let should_mark = (host_iter.marked_changed_mask & bit) == 0;
    let Some(scratch) = host_iter.active_scratch() else {
        return core::ptr::null();
    };

    if should_mark {
        for &tick_cell_ptr in &scratch.tick_ptrs[term_index] {
            // SAFETY: Exclusive mutable access to this sparse component is held by this system.
            unsafe {
                *(*tick_cell_ptr).get() = this_run;
            }
        }
    }

    let out_ptr = scratch.row_ptrs[term_index].as_ptr();
    if should_mark {
        host_iter.marked_changed_mask |= bit;
    }
    out_ptr
}

/// Returns `[ *mut u8; chunk_len ]` of indirect per-row mutable pointers for `StorageType::SparseSet`
/// columns without marking change ticks, for pairing with [`host_iter_mark_changed_ranges`]
/// (ABI `0.4` slot `77`).
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_rows_mutable_untracked(
    iter: *mut BamIter,
    term_index: usize,
) -> *const *mut u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_mut(iter) }) else {
        return core::ptr::null();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.is_sparse_set()
        || (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_immutable()
    {
        return core::ptr::null();
    }
    if (host_iter.present_sparse_mask & (1u32 << term_index)) == 0 {
        return core::ptr::null();
    }
    let Some(scratch) = host_iter.active_scratch() else {
        return core::ptr::null();
    };
    scratch.row_ptrs[term_index].as_ptr()
}

/// Writes [`BamColumnInfo`] metadata for `term_index` into `*out_info`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_info` must point to writable [`BamColumnInfo`] storage, or null.
pub unsafe extern "C" fn host_iter_column_info(
    iter: *const BamIter,
    term_index: usize,
    out_info: *mut BamColumnInfo,
) -> FfiResult {
    if out_info.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_info.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_ref(iter) });

    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let t = &host_iter.plan.terms[term_index];
    let info = BamColumnInfo {
        component: t.component,
        mode: t.mode,
        uuid: t.uuid,
        size: t.size,
        align: t.align,
        storage_flags: t.storage_flags,
        reserved0: 0,
    };
    // SAFETY: `out_info` was checked non-null above and caller guarantees writable `BamColumnInfo`.
    unsafe {
        *out_info = info;
    }
    FfiResult::OK
}

/// Returns a read-only pointer to the singleton `Resource` declared at `term_index`,
/// or null if unauthorized or absent in the `World`.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_res_ptr(iter: *const BamIter, term_index: usize) -> *const u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_ref(iter) }) else {
        return core::ptr::null();
    };
    if term_index >= host_iter.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.mode.is_resource() || !term.is_resource() {
        return core::ptr::null();
    }
    let Some(world) = host_iter.world else {
        return core::ptr::null();
    };
    // SAFETY: `RESOURCE_READ` or `RESOURCE_WRITE` on `term.component_id` was declared in
    // `FilteredAccessSet` during `GuestSystem::initialize`.
    match unsafe { world.get_resource_by_id(term.component_id) } {
        Some(ptr) => ptr.as_ptr(),
        None => core::ptr::null(),
    }
}

/// Returns an exclusive mutable pointer to the singleton `Resource` declared with `RESOURCE_WRITE`
/// at `term_index`, marking its Bevy change tick, or null if unauthorized or absent.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_res_mut_ptr(iter: *mut BamIter, term_index: usize) -> *mut u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) = (unsafe { validate_iter_mut(iter) }) else {
        return core::ptr::null_mut();
    };
    if host_iter.in_par_iter || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null_mut();
    }
    let term = &host_iter.plan.terms[term_index];
    if term.mode != AccessMode::RESOURCE_WRITE || !term.is_resource() || term.is_immutable() {
        return core::ptr::null_mut();
    }
    let Some(world) = host_iter.world else {
        return core::ptr::null_mut();
    };
    // SAFETY: `RESOURCE_WRITE` was declared in `FilteredAccessSet` during `GuestSystem::initialize`,
    // granting exclusive mutable access to `term.component_id` and its resource entity.
    let Some(entity) = (unsafe { world.resource_entities() }).get(term.component_id) else {
        return core::ptr::null_mut();
    };
    let Ok(entity_cell) =
        world.get_entity_with_ticks(entity, host_iter.last_run, host_iter.this_run)
    else {
        return core::ptr::null_mut();
    };
    // SAFETY: Exclusive mutable access to this resource component is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(term.component_id) }) else {
        return core::ptr::null_mut();
    };
    mut_untyped.into_inner().as_ptr()
}

/// Reads up to `max_count` buffered POD Bevy `Messages<M>` from `MESSAGE_READ` / `MESSAGE_WRITE`
/// term `term_index` into `out_buf`, advancing this system's dedicated `MessageCursor<M>`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - If `max_count > 0`, `out_buf` must be writable for `max_count * size_of::<M>()` bytes.
/// - `out_count` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_msg_read_batch(
    iter: *mut BamIter,
    term_index: usize,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult {
    if out_count.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_count.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.mode.is_message() || !term.is_message() {
        return FfiResult::PERMISSION_DENIED;
    }
    let Some(read_fn) = term.msg_read_batch_fn else {
        return FfiResult::INVALID_COMPONENT;
    };
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    let Some(ref mut cursors) = host_iter.msg_cursors else {
        return FfiResult::INVALID_STATE;
    };
    let Some(Some(cursor_box)) = cursors.get_mut(term_index) else {
        return FfiResult::INVALID_STATE;
    };
    // SAFETY: `MESSAGE_READ` or `MESSAGE_WRITE` was declared in `FilteredAccessSet`; `cursor_box`
    // was constructed by `create_msg_cursor_impl::<M>` for `term.component_id`.
    unsafe {
        read_fn(
            world,
            term.component_id,
            cursor_box.as_mut(),
            out_buf,
            max_count,
            out_count,
        )
    }
}

/// Reads all unread buffered POD Bevy `Messages<M>` at `term_index` into a host-managed
/// contiguous scratch slice, writing `(out_ptr, out_count)` (ABI `0.4` slot `88`).
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_ptr` must point to writable `*const u8` storage, or null.
/// - `out_count` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_msg_read_slice(
    iter: *mut BamIter,
    term_index: usize,
    out_ptr: *mut *const u8,
    out_count: *mut usize,
) -> FfiResult {
    // SAFETY: Validates null/alignment and initializes outputs on entry.
    ffi_try!(unsafe { init_out_ptr(out_ptr, core::ptr::null()) });
    // SAFETY: Validates null/alignment and initializes outputs on entry.
    ffi_try!(unsafe { init_out_ptr(out_count, 0) });

    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.mode.is_message() || !term.is_message() {
        return FfiResult::PERMISSION_DENIED;
    }
    let Some(read_fn) = term.msg_read_batch_fn else {
        return FfiResult::INVALID_COMPONENT;
    };
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    let Some(ref mut cursors) = host_iter.msg_cursors else {
        return FfiResult::INVALID_STATE;
    };
    let Some(Some(cursor_box)) = cursors.get_mut(term_index) else {
        return FfiResult::INVALID_STATE;
    };

    // Fast path: dynamic message channels store packed elements contiguously and expose direct
    // zero-copy slice pointers via `dynamic_msg_read_slice`.
    // SAFETY: `MESSAGE_READ` or `MESSAGE_WRITE` declared shared read access to `term.component_id`.
    if let Ok((slice_ptr, slice_count)) = unsafe {
        crate::registry::catalog::dynamic_msg_read_slice(
            world,
            term.component_id,
            cursor_box.as_mut(),
        )
    } {
        // SAFETY: `out_ptr` and `out_count` were verified non-null and aligned above.
        unsafe {
            *out_ptr = slice_ptr;
            *out_count = slice_count;
        }
        return FfiResult::OK;
    }

    if term.size == 0 {
        let mut total_read = 0usize;
        // SAFETY: `term.size == 0` never writes to `out_buf` inside `read_fn`.
        let status = unsafe {
            read_fn(
                world,
                term.component_id,
                cursor_box.as_mut(),
                core::ptr::NonNull::<u8>::dangling().as_ptr(),
                usize::MAX,
                &raw mut total_read,
            )
        };
        if status.is_err() {
            return status;
        }
        if total_read > 0 {
            // SAFETY: `out_ptr` and `out_count` were verified non-null and aligned above.
            unsafe {
                *out_ptr = core::ptr::NonNull::<u8>::dangling().as_ptr();
                *out_count = total_read;
            }
        }
        return FfiResult::OK;
    }

    let scratch = get_or_alloc_scratch(
        &mut host_iter.sparse_scratch,
        &mut host_iter.owned_scratch,
        host_iter.plan.terms.len(),
    );

    let elem_size = term.size;
    let align = term.align.max(1);
    let mut cap_elems =
        (scratch.msg_slice_buf.capacity().saturating_sub(align) / elem_size).max(64);
    let needed_bytes = cap_elems
        .checked_mul(elem_size)
        .and_then(|b| b.checked_add(align))
        .ok_or(FfiResult::CAPACITY_EXCEEDED);
    let Ok(needed) = needed_bytes else {
        return FfiResult::CAPACITY_EXCEEDED;
    };
    if scratch.msg_slice_buf.len() < needed {
        scratch.msg_slice_buf.resize(needed, 0);
    }

    let base_addr = scratch.msg_slice_buf.as_ptr() as usize;
    let mut aligned_off = base_addr.next_multiple_of(align) - base_addr;
    let mut total_read = 0usize;

    loop {
        let batch_cap = cap_elems - total_read;
        let write_ptr = scratch.msg_slice_buf[aligned_off + total_read * elem_size..].as_mut_ptr();
        let mut read_now = 0usize;
        // SAFETY: `write_ptr` points to `batch_cap * elem_size` aligned writable bytes.
        let status = unsafe {
            read_fn(
                world,
                term.component_id,
                cursor_box.as_mut(),
                write_ptr,
                batch_cap,
                &raw mut read_now,
            )
        };
        if status.is_err() {
            return status;
        }
        total_read += read_now;
        if read_now < batch_cap {
            break;
        }

        let new_cap = cap_elems.saturating_mul(2);
        let Some(new_needed) = new_cap
            .checked_mul(elem_size)
            .and_then(|b| b.checked_add(align))
        else {
            return FfiResult::CAPACITY_EXCEEDED;
        };
        // Move already-read bytes to index 0 temporarily in case `resize` shifts allocation alignment.
        let used_bytes = total_read * elem_size;
        if aligned_off != 0 {
            scratch
                .msg_slice_buf
                .copy_within(aligned_off..aligned_off + used_bytes, 0);
        }
        scratch.msg_slice_buf.resize(new_needed, 0);
        let new_base = scratch.msg_slice_buf.as_ptr() as usize;
        let new_aligned_off = new_base.next_multiple_of(align) - new_base;
        if new_aligned_off != 0 {
            scratch
                .msg_slice_buf
                .copy_within(0..used_bytes, new_aligned_off);
        }
        aligned_off = new_aligned_off;
        cap_elems = new_cap;
    }

    if total_read > 0 {
        // SAFETY: `out_ptr` and `out_count` were verified non-null and aligned above.
        unsafe {
            *out_ptr = scratch.msg_slice_buf[aligned_off..].as_ptr();
            *out_count = total_read;
        }
    }
    FfiResult::OK
}

/// Sends a single POD Bevy `Message` into `Messages<M>` declared with `MESSAGE_WRITE` at `term_index`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - If `data_len > 0`, `data_ptr` must be readable for `data_len` bytes.
pub unsafe extern "C" fn host_msg_send(
    iter: *mut BamIter,
    term_index: usize,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if term.mode != AccessMode::MESSAGE_WRITE || !term.is_message() || term.is_immutable() {
        return FfiResult::PERMISSION_DENIED;
    }
    let Some(send_fn) = term.msg_send_fn else {
        return FfiResult::INVALID_COMPONENT;
    };
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    // SAFETY: `MESSAGE_WRITE` was declared in `FilteredAccessSet`; caller guarantees `data_ptr`
    // readability when `data_len > 0`.
    unsafe {
        send_fn(
            world,
            host_iter.last_run,
            host_iter.this_run,
            term.component_id,
            data_ptr,
            data_len,
        )
    }
}

/// Writes `count` contiguous POD messages into `Messages<M>` declared with `MESSAGE_WRITE` at `term_index`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - If `count > 0`, `in_buf` must be readable for `count * size_of::<M>()` bytes.
pub unsafe extern "C" fn host_msg_write_batch(
    iter: *mut BamIter,
    term_index: usize,
    in_buf: *const u8,
    count: usize,
) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if term.mode != AccessMode::MESSAGE_WRITE || !term.is_message() || term.is_immutable() {
        return FfiResult::PERMISSION_DENIED;
    }
    if count == 0 {
        return FfiResult::OK;
    }
    if term.size > 0 && in_buf.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if count.checked_mul(term.size).is_none() {
        return FfiResult::INVALID_ARGUMENT;
    }
    let Some(write_fn) = term.msg_write_batch_fn else {
        return FfiResult::INVALID_COMPONENT;
    };
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    // SAFETY: `MESSAGE_WRITE` was declared in `FilteredAccessSet` and `in_buf` bounds were checked above.
    unsafe {
        write_fn(
            world,
            host_iter.last_run,
            host_iter.this_run,
            term.component_id,
            in_buf,
            count,
        )
    }
}

/// Reads Bevy change-detection ticks (`added`, `changed`, `last_run`, `this_run`) for the
/// singleton `Resource` declared at `term_index`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_ticks` must point to aligned writable [`BamChangeTicks`] storage, or null.
pub unsafe extern "C" fn host_res_ticks(
    iter: *const BamIter,
    term_index: usize,
    out_ticks: *mut BamChangeTicks,
) -> FfiResult {
    // SAFETY: Validates null/alignment and initializes `*out_ticks` on entry.
    let out_slot = ffi_try!(unsafe {
        init_out_ptr(
            out_ticks,
            BamChangeTicks {
                added: 0,
                changed: 0,
                last_run: 0,
                this_run: 0,
            },
        )
    });
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_ref(iter) });
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.mode.is_resource() || !term.is_resource() {
        return FfiResult::PERMISSION_DENIED;
    }
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    // SAFETY: `RESOURCE_READ` or `RESOURCE_WRITE` on `term.component_id` was declared in `FilteredAccessSet`.
    let Some(entity) = (unsafe { world.resource_entities() }).get(term.component_id) else {
        return FfiResult::NOT_FOUND;
    };
    let Ok(entity_cell) = world.get_entity(entity) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: Shared or exclusive access to this resource was declared in `FilteredAccessSet`.
    let Some(ticks) = (unsafe { entity_cell.get_change_ticks_by_id(term.component_id) }) else {
        return FfiResult::NOT_FOUND;
    };
    *out_slot = BamChangeTicks {
        added: ticks.added.get(),
        changed: ticks.changed.get(),
        last_run: host_iter.last_run.get(),
        this_run: host_iter.this_run.get(),
    };
    FfiResult::OK
}

#[derive(Clone, Copy)]
struct ParWorldRefs<'w> {
    tables: &'w Tables,
    archetypes: Option<&'w Archetypes>,
    sparse_sets: Option<&'w SparseSets>,
    world: Option<UnsafeWorldCell<'w>>,
}

// SAFETY: Used strictly within `par_for_each_chunk_impl` scoped worker tasks where:
// 1. Table, archetype, and sparse-set index metadata is immutable during `GuestSystem::run_unsafe`.
// 2. Each task operates on a disjoint `(TableId, start_row..end_row)` or
//    `(ArchetypeId, start_entity..end_entity)` sub-range with task-local `SparseRowScratch`.
// 3. Mutable singleton/message ops (`res_mut_ptr`, `msg_read_batch`, `msg_send`, `msg_write_batch`)
//    are rejected while `in_par_iter == true`, leaving only shared read-only `res_ptr` (`&R: Sync`).
unsafe impl Send for ParWorldRefs<'_> {}
// SAFETY: Same scoped read-only metadata and disjoint-row/entity invariants as `Send`.
unsafe impl Sync for ParWorldRefs<'_> {}

#[derive(Clone, Copy)]
struct DenseParPartition {
    matched_idx: usize,
    table_id: TableId,
    start_row: usize,
    end_row: usize,
}

#[derive(Clone, Copy)]
struct ArchetypeParPartition {
    matched_idx: usize,
    arch_id: ArchetypeId,
    start_entity: usize,
    end_entity: usize,
}

fn dispatch_partitions_slice<P: Send + Sync, S: Send>(
    pool_opt: Option<&ComputeTaskPool>,
    partitions: &[P],
    scratches: &mut [SparseRowScratch],
    storages: &mut [S],
    run: impl Fn(usize, &P, Option<&mut SparseRowScratch>, Option<&mut S>) + Sync,
) {
    let has_scratches = scratches.len() == partitions.len();
    let has_storages = storages.len() == partitions.len();
    if let Some(pool) = pool_opt
        && partitions.len() > 1
    {
        let run_ref = &run;
        pool.scope(|s| match (has_scratches, has_storages) {
            (true, true) => {
                for (idx, ((part, scratch), storage)) in partitions
                    .iter()
                    .zip(scratches.iter_mut())
                    .zip(storages.iter_mut())
                    .enumerate()
                {
                    s.spawn(async move {
                        run_ref(idx, part, Some(scratch), Some(storage));
                    });
                }
            }
            (true, false) => {
                for (idx, (part, scratch)) in
                    partitions.iter().zip(scratches.iter_mut()).enumerate()
                {
                    s.spawn(async move {
                        run_ref(idx, part, Some(scratch), None);
                    });
                }
            }
            (false, true) => {
                for (idx, (part, storage)) in partitions.iter().zip(storages.iter_mut()).enumerate()
                {
                    s.spawn(async move {
                        run_ref(idx, part, None, Some(storage));
                    });
                }
            }
            (false, false) => {
                for (idx, part) in partitions.iter().enumerate() {
                    s.spawn(async move {
                        run_ref(idx, part, None, None);
                    });
                }
            }
        });
    } else {
        match (has_scratches, has_storages) {
            (true, true) => {
                for (idx, ((part, scratch), storage)) in partitions
                    .iter()
                    .zip(scratches.iter_mut())
                    .zip(storages.iter_mut())
                    .enumerate()
                {
                    run(idx, part, Some(scratch), Some(storage));
                }
            }
            (true, false) => {
                for (idx, (part, scratch)) in
                    partitions.iter().zip(scratches.iter_mut()).enumerate()
                {
                    run(idx, part, Some(scratch), None);
                }
            }
            (false, true) => {
                for (idx, (part, storage)) in partitions.iter().zip(storages.iter_mut()).enumerate()
                {
                    run(idx, part, None, Some(storage));
                }
            }
            (false, false) => {
                for (idx, part) in partitions.iter().enumerate() {
                    run(idx, part, None, None);
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ParTaskCallback {
    Basic(GuestFnPtr<BamChunkTaskFn>),
    Ex(GuestFnPtr<BamChunkTaskExFn>),
}

/// Runs one worker partition callback against `sub_iter` (and optional worker `HostCommands`) and
/// folds its result and counters into the shared atomics.
///
/// # Safety
/// `sub_iter` must own a disjoint entity sub-range, and `task` + `ud_ptr` must uphold the
/// thread-safe, nounwind task callback contract.
unsafe fn run_par_sub_iter(
    partition_index: usize,
    sub_iter: &mut HostIter<'_>,
    sub_cmds: Option<&mut HostCommands<'_>>,
    task: ParTaskCallback,
    ud_ptr: GuestUserData,
    first_err: &AtomicU32,
    total_chunks: &AtomicUsize,
    total_entities: &AtomicUsize,
) {
    let rc = match (task, sub_cmds) {
        (ParTaskCallback::Basic(GuestFnPtr(cb)), _) => {
            // SAFETY: Caller guarantees disjoint `sub_iter` slice and thread-safe nounwind `cb`.
            unsafe { cb(sub_iter.as_ffi_ptr(), ud_ptr.as_ptr()) }
        }
        (ParTaskCallback::Ex(GuestFnPtr(cb)), Some(cmds)) => {
            // SAFETY: Caller guarantees disjoint `sub_iter`, local `cmds`, and thread-safe `cb`.
            unsafe {
                cb(
                    sub_iter.as_ffi_ptr(),
                    cmds.as_ffi_ptr(),
                    partition_index,
                    ud_ptr.as_ptr(),
                )
            }
        }
        (ParTaskCallback::Ex(_), None) => FfiResult::INVALID_STATE,
    };
    if rc != FfiResult::OK {
        let _ =
            first_err.compare_exchange(FfiResult::OK.0, rc.0, Ordering::AcqRel, Ordering::Acquire);
    }
    let (c, e) = sub_iter.stats();
    total_chunks.fetch_add(c, Ordering::Relaxed);
    total_entities.fetch_add(e, Ordering::Relaxed);
}

#[inline]
fn compute_par_grain(total_items: usize, worker_count: usize, opts: BamParOptions) -> usize {
    if opts.chunk_size > 0 {
        let mut grain = opts.chunk_size;
        if opts.max_partitions > 0 {
            grain = grain.max(total_items.div_ceil(opts.max_partitions));
        }
        grain.max(1)
    } else {
        let target_parts = if opts.max_partitions > 0 {
            worker_count.min(opts.max_partitions)
        } else {
            worker_count
        }
        .max(1);
        total_items.div_ceil(target_parts).max(1)
    }
}

/// Core parallel chunk iterator backing `par_for_each_chunk`, `par_for_each_chunk_readonly`, and
/// `par_for_each_chunk_ex`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `task` and `user_data` must be thread-safe (`Sync`) across concurrent worker invocations on
///   disjoint entity chunks and must never unwind across `extern "C"`.
unsafe fn par_for_each_chunk_core(
    iter: *mut BamIter,
    task: ParTaskCallback,
    user_data: *mut c_void,
    options: BamParOptions,
    require_readonly: bool,
) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if require_readonly && host_iter.has_writable_terms() {
        return FfiResult::PERMISSION_DENIED;
    }

    let is_ex = matches!(task, ParTaskCallback::Ex(_));
    let par_commands_ptr = host_iter.par_commands;
    if is_ex && par_commands_ptr.is_none() {
        return FfiResult::INVALID_STATE;
    }

    let Some(tables) = host_iter.tables else {
        return FfiResult::OK;
    };
    let plan = host_iter.plan;
    let ticks = RunTicks {
        last_run: host_iter.last_run,
        this_run: host_iter.this_run,
    };
    let ud_ptr = GuestUserData::new(user_data);

    let first_err = AtomicU32::new(FfiResult::OK.0);
    let total_chunks = AtomicUsize::new(0);
    let total_entities = AtomicUsize::new(0);

    // Under Miri, run sequentially on the current thread to avoid spawning OS worker threads while
    // exercising the exact sub-range `HostIter` partitioning and `in_par_iter` invariants.
    let pool_opt: Option<&ComputeTaskPool> = if cfg!(miri) {
        None
    } else {
        Some(ComputeTaskPool::get_or_init(TaskPool::new))
    };
    let worker_count = pool_opt.map_or(1, |p| p.thread_num().max(1));
    let par_gen = crate::handle_header::next_non_zero_generation();

    let par_template = par_commands_ptr.filter(|_| is_ex).map(|ptr| {
        // SAFETY: `par_commands_ptr` points to the parent system's live `HostCommands` and is not
        // mutated concurrently during this synchronous `par_for_each_chunk_core` call.
        let main_cmds = unsafe { ptr.as_ref() };
        (
            main_cmds.par_worker_template(),
            main_cmds.storage.arena_bytes_used(),
        )
    });
    let shared_arena_bytes = par_template
        .as_ref()
        .and_then(|(tpl, used)| tpl.4.max_arena_bytes.map(|_| AtomicUsize::new(*used)));
    let shared_arena_ref = shared_arena_bytes.as_ref();

    let mut fallback_dense_parts = Vec::new();
    let mut fallback_arch_parts = Vec::new();
    let mut fallback_worker_storages = Vec::new();
    let mut fallback_worker_scratches = Vec::new();

    match host_iter.backing {
        IterBacking::ResourceOnly => {
            host_iter.clear_active_chunk();
            return FfiResult::OK;
        }
        IterBacking::DenseTables {
            matched_tables,
            ref mut table_cursor,
            ref mut row_cursor,
            single_table_end_row,
        } => {
            let start_tc = *table_cursor;
            let start_rc = *row_cursor;
            let mut total_rows = 0usize;
            for (rel_idx, &tid) in matched_tables[start_tc..].iter().enumerate() {
                if let Some(t) = tables.get(tid) {
                    let end = single_table_end_row
                        .map(|e| e.min(t.entity_count() as usize))
                        .unwrap_or(t.entity_count() as usize);
                    let start = if rel_idx == 0 { start_rc } else { 0 };
                    total_rows += end.saturating_sub(start);
                }
            }
            *table_cursor = matched_tables.len();
            *row_cursor = 0;
            let world_opt = host_iter.world;
            let table_has_masks = host_iter.has_masks;
            host_iter.clear_active_chunk();
            if total_rows == 0 {
                return FfiResult::OK;
            }

            let mut partitions = if let Some(ref mut scratch) = host_iter.sparse_scratch {
                core::mem::take(&mut scratch.par_dense_partitions)
            } else {
                core::mem::take(&mut fallback_dense_parts)
            };
            partitions.clear();
            let target_grain = compute_par_grain(total_rows, worker_count, options);
            for (rel_idx, &tid) in matched_tables[start_tc..].iter().enumerate() {
                let Some(t) = tables.get(tid) else {
                    continue;
                };
                let full_len = t.entity_count() as usize;
                let end_limit = single_table_end_row
                    .map(|e| e.min(full_len))
                    .unwrap_or(full_len);
                let mut r = if rel_idx == 0 { start_rc } else { 0 };
                while r < end_limit {
                    let end = (r + target_grain).min(end_limit);
                    partitions.push(DenseParPartition {
                        matched_idx: start_tc + rel_idx,
                        table_id: tid,
                        start_row: r,
                        end_row: end,
                    });
                    r = end;
                }
            }

            let part_count = partitions.len();
            let mut worker_scratches = if let Some(ref mut scratch) = host_iter.sparse_scratch {
                core::mem::take(&mut scratch.par_worker_scratches)
            } else {
                core::mem::take(&mut fallback_worker_scratches)
            };
            if worker_scratches.len() < part_count {
                worker_scratches.resize_with(part_count, SparseRowScratch::default);
            }
            for ws in &mut worker_scratches[..part_count] {
                ws.ensure_terms(plan.terms.len());
            }

            let mut worker_storages = if is_ex {
                let mut ws = if let Some(ref mut scratch) = host_iter.sparse_scratch {
                    core::mem::take(&mut scratch.par_worker_storages)
                } else {
                    core::mem::take(&mut fallback_worker_storages)
                };
                if ws.len() < part_count {
                    ws.resize_with(part_count, HostCommandsStorage::new);
                }
                for s in &mut ws[..part_count] {
                    s.clear();
                }
                ws
            } else {
                Vec::new()
            };
            let storage_slice = if is_ex {
                &mut worker_storages[..part_count]
            } else {
                &mut []
            };

            let refs = ParWorldRefs {
                tables,
                archetypes: None,
                sparse_sets: None,
                world: world_opt,
            };
            dispatch_partitions_slice(
                pool_opt,
                &partitions,
                &mut worker_scratches[..part_count],
                storage_slice,
                |part_idx, part, scratch_opt, storage_opt| {
                    if first_err.load(Ordering::Acquire) != FfiResult::OK.0 {
                        return;
                    }
                    let r = refs;
                    let table_slice = core::slice::from_ref(&part.table_id);
                    let table_mask_slice = table_has_masks
                        .get(part.matched_idx)
                        .map_or(&[][..], core::slice::from_ref);
                    let mut sub_iter = HostIter::from_backing(
                        r.tables,
                        IterBacking::DenseTables {
                            matched_tables: table_slice,
                            table_cursor: 0,
                            row_cursor: part.start_row,
                            single_table_end_row: Some(part.end_row),
                        },
                        plan,
                        ticks,
                        r.world,
                        scratch_opt,
                        true,
                    )
                    .with_generation(par_gen)
                    .with_has_masks(table_mask_slice, &[]);
                    let mut sub_cmds_storage = storage_opt.and_then(|storage| {
                        par_template.as_ref().map(
                            |(
                                (
                                    allocator,
                                    snapshot,
                                    mod_id,
                                    is_active,
                                    quota_limits,
                                    shared_spawns,
                                ),
                                _,
                            )| {
                                HostCommands::with_generation(par_gen, storage, allocator, snapshot)
                                    .with_mod_context_ref(*mod_id, *is_active)
                                    .with_quotas(*quota_limits, *shared_spawns)
                                    .with_shared_arena_bytes(shared_arena_ref)
                            },
                        )
                    });
                    // SAFETY: Disjoint `[part.start_row..part.end_row]` slice of `part.table_id` and
                    // dedicated partition `HostCommands` storage.
                    unsafe {
                        run_par_sub_iter(
                            part_idx,
                            &mut sub_iter,
                            sub_cmds_storage.as_mut(),
                            task,
                            ud_ptr,
                            &first_err,
                            &total_chunks,
                            &total_entities,
                        );
                    }
                },
            );

            if is_ex && let Some(cmds_ptr) = par_commands_ptr {
                // SAFETY: `pool.scope` has joined all workers; merge disjoint worker buffers in
                // deterministic `0..part_count` partition order into the parent system's queue.
                let main_cmds = unsafe { &mut *cmds_ptr.as_ptr() };
                let max_arena = par_template
                    .as_ref()
                    .and_then(|(tpl, _)| tpl.4.max_arena_bytes);
                for storage in &mut worker_storages[..part_count] {
                    if let Err(err) = main_cmds.storage.merge_from_bounded(storage, max_arena) {
                        let _ = first_err.compare_exchange(
                            FfiResult::OK.0,
                            err.0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                }
            }
            if let Some(ref mut scratch) = host_iter.sparse_scratch {
                scratch.par_dense_partitions = partitions;
                scratch.par_worker_scratches = worker_scratches;
                if is_ex {
                    scratch.par_worker_storages = worker_storages;
                }
            }
        }
        IterBacking::Archetypes {
            archetypes,
            sparse_sets,
            matched_archetypes,
            ref mut archetype_cursor,
            ref mut entity_cursor,
            single_arch_end_entity,
        } => {
            let start_ac = *archetype_cursor;
            let start_ec = *entity_cursor;
            let mut total_ents = 0usize;
            for (rel_idx, &aid) in matched_archetypes[start_ac..].iter().enumerate() {
                if let Some(a) = archetypes.get(aid) {
                    let end = single_arch_end_entity
                        .map(|e| e.min(a.entities().len()))
                        .unwrap_or(a.entities().len());
                    let start = if rel_idx == 0 { start_ec } else { 0 };
                    total_ents += end.saturating_sub(start);
                }
            }
            *archetype_cursor = matched_archetypes.len();
            *entity_cursor = 0;
            let world_opt = host_iter.world;
            let arch_has_masks = host_iter.has_masks;
            host_iter.clear_active_chunk();
            if total_ents == 0 {
                return FfiResult::OK;
            }

            let mut partitions = if let Some(ref mut scratch) = host_iter.sparse_scratch {
                core::mem::take(&mut scratch.par_arch_partitions)
            } else {
                core::mem::take(&mut fallback_arch_parts)
            };
            partitions.clear();
            let target_grain = compute_par_grain(total_ents, worker_count, options);
            for (rel_idx, &aid) in matched_archetypes[start_ac..].iter().enumerate() {
                let Some(a) = archetypes.get(aid) else {
                    continue;
                };
                let full_len = a.entities().len();
                let end_limit = single_arch_end_entity
                    .map(|e| e.min(full_len))
                    .unwrap_or(full_len);
                let mut idx = if rel_idx == 0 { start_ec } else { 0 };
                while idx < end_limit {
                    let end = (idx + target_grain).min(end_limit);
                    partitions.push(ArchetypeParPartition {
                        matched_idx: start_ac + rel_idx,
                        arch_id: aid,
                        start_entity: idx,
                        end_entity: end,
                    });
                    idx = end;
                }
            }

            let part_count = partitions.len();
            let mut worker_scratches = if let Some(ref mut scratch) = host_iter.sparse_scratch {
                core::mem::take(&mut scratch.par_worker_scratches)
            } else {
                core::mem::take(&mut fallback_worker_scratches)
            };
            if worker_scratches.len() < part_count {
                worker_scratches.resize_with(part_count, SparseRowScratch::default);
            }
            for ws in &mut worker_scratches[..part_count] {
                ws.ensure_terms(plan.terms.len());
            }

            let mut worker_storages = if is_ex {
                let mut ws = if let Some(ref mut scratch) = host_iter.sparse_scratch {
                    core::mem::take(&mut scratch.par_worker_storages)
                } else {
                    core::mem::take(&mut fallback_worker_storages)
                };
                if ws.len() < part_count {
                    ws.resize_with(part_count, HostCommandsStorage::new);
                }
                for s in &mut ws[..part_count] {
                    s.clear();
                }
                ws
            } else {
                Vec::new()
            };
            let storage_slice = if is_ex {
                &mut worker_storages[..part_count]
            } else {
                &mut []
            };

            let refs = ParWorldRefs {
                tables,
                archetypes: Some(archetypes),
                sparse_sets: Some(sparse_sets),
                world: world_opt,
            };
            dispatch_partitions_slice(
                pool_opt,
                &partitions,
                &mut worker_scratches[..part_count],
                storage_slice,
                |part_idx, part, scratch_opt, storage_opt| {
                    if first_err.load(Ordering::Acquire) != FfiResult::OK.0 {
                        return;
                    }
                    let r = refs;
                    let (Some(archetypes_ref), Some(sparse_sets_ref)) =
                        (r.archetypes, r.sparse_sets)
                    else {
                        return;
                    };
                    let arch_slice = core::slice::from_ref(&part.arch_id);
                    let arch_mask_slice = arch_has_masks
                        .get(part.matched_idx)
                        .map_or(&[][..], core::slice::from_ref);
                    let mut sub_iter = HostIter::from_backing(
                        r.tables,
                        IterBacking::Archetypes {
                            archetypes: archetypes_ref,
                            sparse_sets: sparse_sets_ref,
                            matched_archetypes: arch_slice,
                            archetype_cursor: 0,
                            entity_cursor: part.start_entity,
                            single_arch_end_entity: Some(part.end_entity),
                        },
                        plan,
                        ticks,
                        r.world,
                        scratch_opt,
                        true,
                    )
                    .with_generation(par_gen)
                    .with_has_masks(&[], arch_mask_slice);
                    let mut sub_cmds_storage = storage_opt.and_then(|storage| {
                        par_template.as_ref().map(
                            |(
                                (
                                    allocator,
                                    snapshot,
                                    mod_id,
                                    is_active,
                                    quota_limits,
                                    shared_spawns,
                                ),
                                _,
                            )| {
                                HostCommands::with_generation(par_gen, storage, allocator, snapshot)
                                    .with_mod_context_ref(*mod_id, *is_active)
                                    .with_quotas(*quota_limits, *shared_spawns)
                                    .with_shared_arena_bytes(shared_arena_ref)
                            },
                        )
                    });
                    // SAFETY: Disjoint `[part.start_entity..part.end_entity]` slice with task-local
                    // scratch and dedicated partition `HostCommands` storage.
                    unsafe {
                        run_par_sub_iter(
                            part_idx,
                            &mut sub_iter,
                            sub_cmds_storage.as_mut(),
                            task,
                            ud_ptr,
                            &first_err,
                            &total_chunks,
                            &total_entities,
                        );
                    }
                },
            );

            if is_ex && let Some(cmds_ptr) = par_commands_ptr {
                // SAFETY: `pool.scope` has joined all workers; merge disjoint worker buffers in
                // deterministic `0..part_count` partition order into the parent system's queue.
                let main_cmds = unsafe { &mut *cmds_ptr.as_ptr() };
                let max_arena = par_template
                    .as_ref()
                    .and_then(|(tpl, _)| tpl.4.max_arena_bytes);
                for storage in &mut worker_storages[..part_count] {
                    if let Err(err) = main_cmds.storage.merge_from_bounded(storage, max_arena) {
                        let _ = first_err.compare_exchange(
                            FfiResult::OK.0,
                            err.0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                    }
                }
            }
            if let Some(ref mut scratch) = host_iter.sparse_scratch {
                scratch.par_arch_partitions = partitions;
                scratch.par_worker_scratches = worker_scratches;
                if is_ex {
                    scratch.par_worker_storages = worker_storages;
                }
            }
        }
    }

    host_iter.chunks_yielded += total_chunks.load(Ordering::Relaxed);
    host_iter.entities_yielded += total_entities.load(Ordering::Relaxed);
    FfiResult(first_err.load(Ordering::Acquire))
}

/// Executes `chunk_fn(task_iter, user_data)` in parallel across Bevy's [`ComputeTaskPool`] over
/// disjoint matched table/archetype sub-ranges with zero command-arena allocations per worker.
///
/// When `require_readonly` is `true`, rejects any query plan declaring a mutable term (`WRITE`,
/// `OPTIONAL_WRITE`, `RESOURCE_WRITE`, or `MESSAGE_WRITE`) with [`FfiResult::PERMISSION_DENIED`].
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `chunk_fn` and `user_data` must be thread-safe (`Sync`) across concurrent worker invocations
///   on disjoint entity chunks and must never unwind across `extern "C"`.
pub(crate) unsafe fn par_for_each_chunk_impl(
    iter: *mut BamIter,
    chunk_fn: Option<BamChunkTaskFn>,
    user_data: *mut c_void,
    require_readonly: bool,
) -> FfiResult {
    let Some(task_cb) = chunk_fn else {
        return FfiResult::NULL_POINTER;
    };
    // SAFETY: Forwarding under identical FFI safety contract.
    unsafe {
        par_for_each_chunk_core(
            iter,
            ParTaskCallback::Basic(GuestFnPtr(task_cb)),
            user_data,
            BamParOptions::new(),
            require_readonly,
        )
    }
}

/// Executes `chunk_fn(task_iter, user_data)` in parallel across Bevy's [`ComputeTaskPool`] over
/// disjoint matched table/archetype sub-ranges.
///
/// Each worker task receives a scoped `BamIter` yielding only its disjoint sub-range via
/// `iter_next_chunk`. Non-reentrant (`in_par_iter == true` rejects nested `par_for_each_chunk`
/// with `ACCESS_CONFLICT`), and any non-`OK` `FfiResult` returned by a task (including
/// `PANIC_CAUGHT`) aborts remaining chunk dispatches and is returned deterministically.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `chunk_fn` and `user_data` must be thread-safe (`Sync`) across concurrent worker invocations
///   on disjoint entity chunks.
pub unsafe extern "C" fn host_par_for_each_chunk(
    iter: *mut BamIter,
    chunk_fn: Option<BamChunkTaskFn>,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: Caller upholds `iter`, `chunk_fn`, and `user_data` safety contract.
    unsafe { par_for_each_chunk_impl(iter, chunk_fn, user_data, false) }
}

/// Executes `chunk_fn(task_iter, worker_commands, partition_index, user_data)` in parallel across
/// Bevy's [`ComputeTaskPool`] with optional [`BamParOptions`] tuning and deterministic post-join
/// command-buffer merging (ABI `0.4` slot `81`).
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `chunk_fn` and `user_data` must be thread-safe (`Sync`) across concurrent worker invocations
///   on disjoint entity chunks and must never unwind across `extern "C"`.
/// - If `options` is non-null, it must point to a valid aligned [`BamParOptions`] with
///   `struct_size` initialized.
pub unsafe extern "C" fn host_par_for_each_chunk_ex(
    iter: *mut BamIter,
    chunk_fn: Option<BamChunkTaskExFn>,
    user_data: *mut c_void,
    options: *const BamParOptions,
) -> FfiResult {
    let Some(task_cb) = chunk_fn else {
        return FfiResult::NULL_POINTER;
    };
    let opts = if options.is_null() {
        BamParOptions::new()
    } else {
        if !options.is_aligned() {
            return FfiResult::INVALID_ARGUMENT;
        }
        // SAFETY: `options` is non-null and aligned for `u32`; `struct_size` is at offset 0.
        let struct_size = unsafe { core::ptr::read(options.cast::<u32>()) } as usize;
        if struct_size < BamParOptions::MIN_V0_4_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        // SAFETY: `options` is non-null, aligned, and at least `size_of::<BamParOptions>()` bytes.
        let parsed = unsafe { *options };
        let status = parsed.validate();
        if status.is_err() {
            return status;
        }
        parsed
    };
    // SAFETY: Caller upholds `iter`, `chunk_fn`, and `user_data` safety contract.
    unsafe {
        par_for_each_chunk_core(
            iter,
            ParTaskCallback::Ex(GuestFnPtr(task_cb)),
            user_data,
            opts,
            false,
        )
    }
}
