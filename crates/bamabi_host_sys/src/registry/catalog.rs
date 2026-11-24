//! Copy-on-Write (`CoW`) component, resource, VTable, and `"bam.v0.*"` symbol catalog.

use crate::ffi::assets_scene::{HostAssetBackend, HostSceneBackend};
pub use crate::ffi::relations::{
    ModRelationGraph, RelationEdgeView, RelationFlushError, RelationFlushPolicy,
    check_relation_acyclic, ensure_relation_graph_in_world,
};
use crate::loader::ModId;
use crate::registry::bridge::ExtractRelationshipTargetsFn;
use crate::registry::collision::ComponentCollisionError;
use crate::sync_ptr::SymbolAddress;
use arc_swap::ArcSwap;
use bamabi_sys::{
    AbiPod, BAM_CHILD_OF_NAME, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
    BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_RELATION_FLAG_ACYCLIC, BAM_RELATION_FLAG_CASCADE_DESPAWN,
    BAM_RELATION_FLAG_CLEANUP_REMOVE, BAM_RELATION_FLAG_EXCLUSIVE, BAM_RELATION_FLAG_SYMMETRIC,
    BAM_STORAGE_FLAG_ENTITY_EVENT, BAM_STORAGE_FLAG_EVENT, BAM_STORAGE_FLAG_IMMUTABLE,
    BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET,
    BAM_STORAGE_FLAG_SUMMARY_TICK, BamComponent, BamFieldDesc, ComponentDesc, ComponentDropFn,
    ComponentUuid, FfiChildOf, FfiEntity, FfiResult, FfiSlice, ModComponent, RawInsertable,
    StrView,
};
use bevy_ecs::change_detection::{MaybeLocation, Tick};
use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, ComponentId, StorageType};
use bevy_ecs::entity::{ContainsEntity, Entity};
use bevy_ecs::event::{EntityEvent, Event, EventKey};
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::message::{Message, MessageCursor, Messages};
use bevy_ecs::prelude::{Component, Resource};
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::world::World;
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy_platform::collections::HashMap;
use core::alloc::Layout;
use core::any::Any;
use core::ffi::c_void;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Function signature for monomorphized raw component byte insertion into a Bevy [`World`].
///
/// Takes a raw `*const u8` pointer (backed by `MaybeUninit<u8>` arena storage) rather than `&[u8]`
/// so uninitialized struct padding bytes never form a Rust `[u8]` slice.
pub type InsertRawFn = unsafe fn(world: &mut World, entity: Entity, data: *const u8);

/// Function signature for monomorphized `Default` component insertion into a Bevy [`World`].
pub type InsertDefaultFn = fn(world: &mut World, entity: Entity);

/// Monomorphized factory creating a per-system type-erased `MessageCursor<M>`.
pub type CreateMsgCursorFn = fn() -> Box<dyn Any + Send + Sync>;

/// Monomorphized batch reader advancing a per-system `MessageCursor<M>` over `Messages<M>`.
///
/// # Safety
/// - `world` must hold shared read (or write) access to `msg_res_id` (`Messages<M>`).
/// - `cursor` must have been created by the matching [`CreateMsgCursorFn`].
/// - If `max_count > 0`, `out_buf` must be writable for `max_count * size_of::<M>()` bytes.
/// - `out_count` must be non-null and writable.
pub type MsgReadBatchFn = unsafe fn(
    world: UnsafeWorldCell<'_>,
    msg_res_id: ComponentId,
    cursor: &mut dyn Any,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult;

/// Monomorphized message writer pushing a single POD `M` into `Messages<M>`.
///
/// # Safety
/// - `world` must hold exclusive write access to `msg_res_id` (`Messages<M>`).
/// - If `data_len > 0`, `data_ptr` must be readable for `data_len` bytes.
pub type MsgSendFn = unsafe fn(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult;

/// Function signature for monomorphized batch `Messages<M>` writes across the C ABI.
pub type MsgWriteBatchFn = unsafe fn(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    count: usize,
) -> FfiResult;

/// Function signature for monomorphized typed `world.trigger(...)` invocation from raw guest bytes.
pub type TriggerEventFn =
    unsafe fn(world: &mut World, target: Option<Entity>, data: *const u8) -> Result<(), FfiResult>;

/// Safe function signature triggering an entity-only host `EntityEvent` (such as `bevy_scene::Ready`)
/// directly from a validated target [`Entity`] without requiring `From<Entity>`.
pub type EntityEventTriggerFn = fn(world: &mut World, entity: Entity);

/// Function signature extracting the target [`Entity`] from a raw `EntityEvent` payload.
pub type ExtractEventTargetFn = unsafe fn(data: *const u8) -> Option<Entity>;

/// ABI-stable `#[repr(transparent)]` entity wrapper implementing Bevy's [`ContainsEntity`] trait
/// for `#[event_target]` fields in `#[derive(EntityEvent, ModComponent)]` host event structs.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TargetEntity(pub FfiEntity);

impl Default for TargetEntity {
    #[inline]
    fn default() -> Self {
        Self(FfiEntity::NULL)
    }
}

impl TargetEntity {
    /// Wraps a Bevy [`Entity`] as a `TargetEntity`.
    #[inline]
    pub const fn from_entity(entity: Entity) -> Self {
        Self(FfiEntity::from_bits(entity.to_bits()))
    }

    /// Converts back to a Bevy [`Entity`], or `Entity::PLACEHOLDER` if [`FfiEntity::NULL`].
    #[inline]
    pub const fn to_entity(self) -> Entity {
        if let Some(e) = Entity::try_from_bits(self.0.to_bits()) {
            e
        } else {
            Entity::PLACEHOLDER
        }
    }
}

impl From<Entity> for TargetEntity {
    #[inline]
    fn from(entity: Entity) -> Self {
        Self::from_entity(entity)
    }
}

impl From<FfiEntity> for TargetEntity {
    #[inline]
    fn from(entity: FfiEntity) -> Self {
        Self(entity)
    }
}

impl From<TargetEntity> for FfiEntity {
    #[inline]
    fn from(entity: TargetEntity) -> Self {
        entity.0
    }
}

impl core::ops::Deref for TargetEntity {
    type Target = FfiEntity;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ContainsEntity for TargetEntity {
    #[inline]
    fn entity(&self) -> Entity {
        self.to_entity()
    }
}

// SAFETY: `TargetEntity` is `#[repr(transparent)]` over `FfiEntity` (`u64`), has no padding or drop
// glue, and matches `ModComponent` field classification for `TargetEntity`.
unsafe impl RawInsertable for TargetEntity {}

// SAFETY: `TargetEntity` is `#[repr(transparent)]` over `FfiEntity`, which is `AbiPod`.
unsafe impl AbiPod for TargetEntity {}

fn create_msg_cursor_impl<M: Message>() -> Box<dyn Any + Send + Sync> {
    Box::new(MessageCursor::<M>::default())
}

/// Cursor tracking the next unread element index within a [`DynamicMessageBuffer`].
#[derive(Default)]
pub(crate) struct DynamicMessageCursor {
    next_index: usize,
}

/// Type-erased, alignment-preserving dynamic message ring/buffer stored as a Bevy resource
/// for guest-registered [`host_register_message`](crate::ffi::register) channels.
pub(crate) struct DynamicMessageBuffer {
    elem_size: usize,
    elem_align: usize,
    stride: usize,
    len: usize,
    cap: usize,
    data: NonNull<u8>,
    drop_fn: Option<ComponentDropFn>,
    is_active: Arc<AtomicBool>,
}

// SAFETY: Access to `DynamicMessageBuffer` follows Bevy's ECS resource borrowing rules (`&` vs
// `&mut`), and `is_active` is synchronized atomically.
unsafe impl Send for DynamicMessageBuffer {}
// SAFETY: Shared references only read immutable byte slices within `[0 .. len * stride)`.
unsafe impl Sync for DynamicMessageBuffer {}

impl DynamicMessageBuffer {
    fn new(
        elem_size: usize,
        elem_align: usize,
        drop_fn: Option<ComponentDropFn>,
        is_active: Arc<AtomicBool>,
    ) -> Self {
        let align = elem_align.max(1);
        let stride = if elem_size == 0 {
            0
        } else {
            elem_size.next_multiple_of(align)
        };
        Self {
            elem_size,
            elem_align: align,
            stride,
            len: 0,
            cap: 0,
            data: NonNull::dangling(),
            drop_fn,
            is_active,
        }
    }

    fn refresh_drop_binding(
        &mut self,
        drop_fn: Option<ComponentDropFn>,
        is_active: Arc<AtomicBool>,
    ) {
        self.clear();
        self.drop_fn = drop_fn;
        self.is_active = is_active;
    }

    fn clear(&mut self) {
        if let Some(df) = self.drop_fn
            && self.is_active.load(Ordering::Acquire)
        {
            for idx in 0..self.len {
                let offset = idx.saturating_mul(self.stride);
                // SAFETY: `idx < self.len <= self.cap` and `offset` stays within the live
                // initialized element buffer while `self.is_active` confirms `df` is loaded.
                unsafe {
                    df(self.data.as_ptr().add(offset).cast::<c_void>());
                }
            }
        }
        self.len = 0;
    }

    fn ensure_additional(&mut self, additional: usize) -> Result<(), FfiResult> {
        if additional == 0 || self.stride == 0 {
            return Ok(());
        }
        let needed = self
            .len
            .checked_add(additional)
            .ok_or(FfiResult::CAPACITY_EXCEEDED)?;
        if needed <= self.cap {
            return Ok(());
        }
        let new_cap = needed.max(self.cap.saturating_mul(2)).max(8);
        let new_bytes = new_cap
            .checked_mul(self.stride)
            .ok_or(FfiResult::CAPACITY_EXCEEDED)?;
        if new_bytes > isize::MAX as usize {
            return Err(FfiResult::CAPACITY_EXCEEDED);
        }
        let new_layout = Layout::from_size_align(new_bytes, self.elem_align)
            .map_err(|_| FfiResult::INVALID_ARGUMENT)?;
        // SAFETY: `new_bytes > 0` (since `self.stride > 0` and `new_cap >= 8`) and `new_layout`
        // is valid and non-zero-sized.
        let raw_new = unsafe { std::alloc::alloc(new_layout) };
        let Some(new_ptr) = NonNull::new(raw_new) else {
            return Err(FfiResult::CAPACITY_EXCEEDED);
        };
        if self.len > 0 {
            let live_bytes = self.len.saturating_mul(self.stride);
            // SAFETY: Both `self.data` and `new_ptr` are valid, non-overlapping allocations of at
            // least `live_bytes`, and guest types moved via memcpy relocation match Rust move rules.
            unsafe {
                core::ptr::copy_nonoverlapping(self.data.as_ptr(), new_ptr.as_ptr(), live_bytes);
            }
        }
        if self.cap > 0 {
            let old_bytes = self.cap.saturating_mul(self.stride);
            if let Ok(old_layout) = Layout::from_size_align(old_bytes, self.elem_align) {
                // SAFETY: `self.data` was allocated with `old_layout`.
                unsafe {
                    std::alloc::dealloc(self.data.as_ptr(), old_layout);
                }
            }
        }
        self.data = new_ptr;
        self.cap = new_cap;
        Ok(())
    }

    /// # Safety
    ///
    /// If `self.elem_size > 0 && count > 0`, `src_ptr` must be non-null and readable for
    /// `count * self.elem_size` tightly packed element bytes.
    unsafe fn push_packed(&mut self, src_ptr: *const u8, count: usize) -> FfiResult {
        if count == 0 {
            return FfiResult::OK;
        }
        if self.elem_size > 0 && src_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if let Err(err) = self.ensure_additional(count) {
            return err;
        }
        if self.elem_size > 0 {
            let base_len = self.len;
            if self.stride == self.elem_size {
                let total_bytes = count.saturating_mul(self.elem_size);
                // SAFETY: Capacity for `base_len + count` elements is allocated; `src_ptr` is
                // non-null and readable for `total_bytes`.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        src_ptr,
                        self.data.as_ptr().add(base_len * self.stride),
                        total_bytes,
                    );
                }
            } else {
                for i in 0..count {
                    // SAFETY: Caller guarantees `src_ptr` holds `count * self.elem_size` bytes, and
                    // destination slot `(base_len + i) * self.stride` is within `self.cap`.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            src_ptr.add(i * self.elem_size),
                            self.data.as_ptr().add((base_len + i) * self.stride),
                            self.elem_size,
                        );
                    }
                }
            }
        }
        self.len = self.len.saturating_add(count);
        FfiResult::OK
    }

    fn read_slice_cursor(&self, cursor: &mut DynamicMessageCursor) -> (*const u8, usize) {
        if cursor.next_index >= self.len {
            return (core::ptr::null(), 0);
        }
        let count = self.len - cursor.next_index;
        let ptr = if self.stride == 0 {
            NonNull::<u8>::dangling().as_ptr()
        } else {
            // SAFETY: `cursor.next_index < self.len <= self.cap`, so offset is in bounds.
            unsafe { self.data.as_ptr().add(cursor.next_index * self.stride) }
        };
        cursor.next_index = self.len;
        (ptr, count)
    }
}

impl Drop for DynamicMessageBuffer {
    fn drop(&mut self) {
        self.clear();
        if self.cap > 0 && self.stride > 0 {
            let old_bytes = self.cap.saturating_mul(self.stride);
            if let Ok(layout) = Layout::from_size_align(old_bytes, self.elem_align) {
                // SAFETY: `self.data` was allocated via `std::alloc::alloc` with `layout`.
                unsafe {
                    std::alloc::dealloc(self.data.as_ptr(), layout);
                }
            }
        }
    }
}

/// # Safety
///
/// `ptr` must own a properly aligned, initialized `DynamicMessageBuffer`.
unsafe fn drop_dynamic_message_buffer(ptr: OwningPtr<'_>) {
    // SAFETY: Caller transfers ownership of a valid `DynamicMessageBuffer` stored in Bevy's table.
    let _ = unsafe { ptr.read::<DynamicMessageBuffer>() };
}

fn dynamic_create_msg_cursor() -> Box<dyn Any + Send + Sync> {
    Box::new(DynamicMessageCursor::default())
}

/// # Safety
///
/// - `world` must grant shared read access to `msg_res_id` (`DynamicMessageBuffer`).
/// - `out_count` must be non-null and writable for `usize`.
/// - If `max_count > 0`, `out_buf` must be writable for `max_count * elem_size` bytes.
unsafe fn dynamic_msg_read_batch(
    world: UnsafeWorldCell<'_>,
    msg_res_id: ComponentId,
    cursor: &mut dyn Any,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult {
    // SAFETY: Caller guarantees `out_count` is non-null and writable.
    unsafe {
        *out_count = 0;
    }
    let Some(typed_cursor) = cursor.downcast_mut::<DynamicMessageCursor>() else {
        return FfiResult::TYPE_MISMATCH;
    };
    if max_count == 0 {
        return FfiResult::OK;
    }
    // SAFETY: `MESSAGE_READ` or `MESSAGE_WRITE` on `msg_res_id` was declared in `FilteredAccessSet`.
    let Some(res_ptr) = (unsafe { world.get_resource_by_id(msg_res_id) }) else {
        return FfiResult::OK;
    };
    // SAFETY: `msg_res_id` was registered with `DynamicMessageBuffer` layout.
    let buffer = unsafe { res_ptr.deref::<DynamicMessageBuffer>() };
    if buffer.elem_size > 0 && out_buf.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if typed_cursor.next_index >= buffer.len {
        return FfiResult::OK;
    }
    let available = buffer.len - typed_cursor.next_index;
    let effective_max = clamp_batch_max_count(max_count, buffer.elem_size);
    let to_copy = available.min(effective_max);
    if buffer.elem_size > 0 && to_copy > 0 {
        let start = typed_cursor.next_index;
        if buffer.stride == buffer.elem_size {
            let total_bytes = to_copy.saturating_mul(buffer.elem_size);
            // SAFETY: `start + to_copy <= buffer.len <= buffer.cap` and `out_buf` is writable for
            // `effective_max * buffer.elem_size >= total_bytes` bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buffer.data.as_ptr().add(start * buffer.stride),
                    out_buf,
                    total_bytes,
                );
            }
        } else {
            for i in 0..to_copy {
                // SAFETY: Source and destination offsets stay within allocated bounds.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buffer.data.as_ptr().add((start + i) * buffer.stride),
                        out_buf.add(i * buffer.elem_size),
                        buffer.elem_size,
                    );
                }
            }
        }
    }
    typed_cursor.next_index = typed_cursor.next_index.saturating_add(to_copy);
    // SAFETY: `out_count` is non-null and writable.
    unsafe {
        *out_count = to_copy;
    }
    FfiResult::OK
}

/// # Safety
///
/// - `world` must grant exclusive write access to `msg_res_id` (`DynamicMessageBuffer`).
/// - If `data_len > 0`, `data_ptr` must be readable for `data_len` bytes.
unsafe fn dynamic_msg_send(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    // SAFETY: `MESSAGE_WRITE` on `msg_res_id` was declared in `FilteredAccessSet`.
    let Some(entity) = (unsafe { world.resource_entities() }).get(msg_res_id) else {
        return FfiResult::NOT_FOUND;
    };
    let Ok(entity_cell) = world.get_entity_with_ticks(entity, last_run, this_run) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: Exclusive mutable access to `msg_res_id` is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(msg_res_id) }) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: `msg_res_id` was registered with `DynamicMessageBuffer` layout.
    let mut buffer = unsafe { mut_untyped.with_type::<DynamicMessageBuffer>() };
    if data_len != buffer.elem_size {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `data_ptr` is verified readable for `data_len == buffer.elem_size` bytes inside `push_packed`.
    unsafe { buffer.push_packed(data_ptr, 1) }
}

/// # Safety
///
/// - `world` must grant exclusive write access to `msg_res_id` (`DynamicMessageBuffer`).
/// - If `count > 0`, `data_ptr` must be readable for `count * elem_size` bytes.
unsafe fn dynamic_msg_write_batch(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    count: usize,
) -> FfiResult {
    if count == 0 {
        return FfiResult::OK;
    }
    // SAFETY: `MESSAGE_WRITE` on `msg_res_id` was declared in `FilteredAccessSet`.
    let Some(entity) = (unsafe { world.resource_entities() }).get(msg_res_id) else {
        return FfiResult::NOT_FOUND;
    };
    let Ok(entity_cell) = world.get_entity_with_ticks(entity, last_run, this_run) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: Exclusive mutable access to `msg_res_id` is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(msg_res_id) }) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: `msg_res_id` was registered with `DynamicMessageBuffer` layout.
    let mut buffer = unsafe { mut_untyped.with_type::<DynamicMessageBuffer>() };
    // SAFETY: Caller guarantees `data_ptr` holds `count * buffer.elem_size` readable bytes.
    unsafe { buffer.push_packed(data_ptr, count) }
}

/// Zero-copy slice reader for dynamic message channels created via `register_message_from_desc`.
///
/// Returns `Ok((ptr, count))` borrowing contiguous elements directly from `DynamicMessageBuffer`
/// and advances `cursor`. Returns `Err(FfiResult::UNSUPPORTED)` when `stride != elem_size`.
///
/// # Safety
///
/// - `world` must grant shared read access to `msg_res_id` (`DynamicMessageBuffer`).
pub(crate) unsafe fn dynamic_msg_read_slice(
    world: UnsafeWorldCell<'_>,
    msg_res_id: ComponentId,
    cursor: &mut dyn Any,
) -> Result<(*const u8, usize), FfiResult> {
    let Some(typed_cursor) = cursor.downcast_mut::<DynamicMessageCursor>() else {
        return Err(FfiResult::TYPE_MISMATCH);
    };
    // SAFETY: Caller guarantees shared read access to `msg_res_id`.
    let Some(res_ptr) = (unsafe { world.get_resource_by_id(msg_res_id) }) else {
        return Ok((core::ptr::null(), 0));
    };
    // SAFETY: `msg_res_id` was registered with `DynamicMessageBuffer` layout.
    let buffer = unsafe { res_ptr.deref::<DynamicMessageBuffer>() };
    if buffer.stride != buffer.elem_size {
        return Err(FfiResult::UNSUPPORTED);
    }
    Ok(buffer.read_slice_cursor(typed_cursor))
}

/// Clamps a batch `max_count` so `max_count * elem_size` never exceeds `isize::MAX` bytes.
#[inline]
pub(crate) const fn clamp_batch_max_count(max_count: usize, elem_size: usize) -> usize {
    if let Some(max_elems) = (isize::MAX as usize).checked_div(elem_size) {
        if max_count < max_elems {
            max_count
        } else {
            max_elems
        }
    } else {
        max_count
    }
}

/// Drains at most `max_count` items from `iter` without advancing past the boundary.
///
/// Uses [`Iterator::take`] so iterators whose `next()` advances cursor state before yielding
/// (such as Bevy's `MessageCursor::read`) never consume and silently drop an extra item when
/// stopping at `max_count`.
#[inline]
pub(crate) fn drain_cursor_bounded<T, I: Iterator<Item = T>>(
    iter: I,
    max_count: usize,
    mut visit: impl FnMut(usize, T),
) -> usize {
    let mut written = 0usize;
    for (idx, item) in iter.take(max_count).enumerate() {
        visit(idx, item);
        written = idx + 1;
    }
    written
}

/// # Safety
///
/// - `world` must grant shared read access to `msg_res_id` (`Messages<M>`).
/// - `out_count` must be non-null and writable for `usize`.
/// - If `max_count > 0` and `size_of::<M>() > 0`, `out_buf` must be writable for
///   `max_count * size_of::<M>()` bytes.
unsafe fn msg_read_batch_impl<M: Message + AbiPod>(
    world: UnsafeWorldCell<'_>,
    msg_res_id: ComponentId,
    cursor: &mut dyn Any,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult {
    // SAFETY: Caller guarantees `out_count` is non-null and writable.
    unsafe {
        *out_count = 0;
    }
    let Some(typed_cursor) = cursor.downcast_mut::<MessageCursor<M>>() else {
        return FfiResult::TYPE_MISMATCH;
    };
    if max_count == 0 {
        return FfiResult::OK;
    }
    let elem_size = core::mem::size_of::<M>();
    if elem_size > 0 && out_buf.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `MESSAGE_READ` or `MESSAGE_WRITE` on `msg_res_id` (`Messages<M>`) was declared in `FilteredAccessSet`.
    let Some(res_ptr) = (unsafe { world.get_resource_by_id(msg_res_id) }) else {
        return FfiResult::OK;
    };
    // SAFETY: `msg_res_id` was registered from `world.register_component::<Messages<M>>()`.
    let messages: &Messages<M> = unsafe { res_ptr.deref::<Messages<M>>() };
    let effective_max = clamp_batch_max_count(max_count, elem_size);
    let written = drain_cursor_bounded(typed_cursor.read(messages), effective_max, |idx, msg| {
        if elem_size > 0 {
            // SAFETY: `idx < effective_max <= max_count` and `effective_max * elem_size <= isize::MAX`,
            // so `out_buf.add(idx * elem_size)` stays within `out_buf`'s `max_count * size_of::<M>()`
            // writable byte allocation without `isize` offset overflow; `write_unaligned`
            // supports arbitrary guest buffer alignments cleanly.
            unsafe {
                core::ptr::write_unaligned(out_buf.add(idx * elem_size).cast::<M>(), *msg);
            }
        }
    });
    // SAFETY: `out_count` is non-null and writable.
    unsafe {
        *out_count = written;
    }
    FfiResult::OK
}

/// Pushes a constructed `msg: M` into the World's `Messages<M>` resource, stamping `this_run`.
///
/// # Safety
/// Caller must guarantee `MESSAGE_WRITE` on `msg_res_id` was declared in `FilteredAccessSet`.
unsafe fn push_message_to_world<M: Message>(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    msg: M,
) -> FfiResult {
    // SAFETY: `MESSAGE_WRITE` on `msg_res_id` (`Messages<M>`) was declared in `FilteredAccessSet`,
    // granting exclusive mutable access to `Messages<M>` and its resource entity.
    let Some(entity) = (unsafe { world.resource_entities() }).get(msg_res_id) else {
        return FfiResult::NOT_FOUND;
    };
    let Ok(entity_cell) = world.get_entity_with_ticks(entity, last_run, this_run) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: Exclusive mutable access to `Messages<M>` is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(msg_res_id) }) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: `msg_res_id` was registered for `Messages<M>`.
    let mut messages = unsafe { mut_untyped.with_type::<Messages<M>>() };
    messages.write(msg);
    FfiResult::OK
}

/// # Safety
///
/// - `world` must grant exclusive write access to `msg_res_id` (`Messages<M>`).
/// - If `data_len > 0`, `data_ptr` must be null or readable for `size_of::<M>()` bytes.
unsafe fn msg_send_impl<M: Message + AbiPod>(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if data_len != core::mem::size_of::<M>() {
        return FfiResult::INVALID_ARGUMENT;
    }
    if data_len > 0 && data_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let msg: M = if data_len == 0 {
        // SAFETY: `M: AbiPod` is zero-sized, `Copy`, and valid when zeroed.
        unsafe { core::mem::MaybeUninit::<M>::zeroed().assume_init() }
    } else {
        // SAFETY: `data_ptr` is non-null and readable for `size_of::<M>()` bytes.
        unsafe { core::ptr::read_unaligned(data_ptr.cast::<M>()) }
    };
    // SAFETY: Caller declared `MESSAGE_WRITE` on `msg_res_id`.
    unsafe { push_message_to_world(world, last_run, this_run, msg_res_id, msg) }
}

/// # Safety
///
/// - `world` must grant shared read access to `msg_res_id` (`Messages<M>`).
/// - `out_count` must be non-null and writable for `usize`.
/// - If `max_count > 0`, `out_buf` must be writable for `max_count * size_of::<*const c_void>()` bytes.
unsafe fn msg_read_batch_non_pod_impl<M: Message>(
    world: UnsafeWorldCell<'_>,
    msg_res_id: ComponentId,
    cursor: &mut dyn Any,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult {
    // SAFETY: Caller guarantees `out_count` is non-null and writable.
    unsafe {
        *out_count = 0;
    }
    let Some(typed_cursor) = cursor.downcast_mut::<MessageCursor<M>>() else {
        return FfiResult::TYPE_MISMATCH;
    };
    if max_count == 0 {
        return FfiResult::OK;
    }
    if out_buf.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `MESSAGE_READ` or `MESSAGE_WRITE` on `msg_res_id` (`Messages<M>`) was declared in `FilteredAccessSet`.
    let Some(res_ptr) = (unsafe { world.get_resource_by_id(msg_res_id) }) else {
        return FfiResult::OK;
    };
    // SAFETY: `msg_res_id` was registered from `world.register_component::<Messages<M>>()`.
    let messages: &Messages<M> = unsafe { res_ptr.deref::<Messages<M>>() };
    let ptr_size = core::mem::size_of::<*const c_void>();
    let effective_max = clamp_batch_max_count(max_count, ptr_size);
    let written = drain_cursor_bounded(typed_cursor.read(messages), effective_max, |idx, msg| {
        let msg_ptr = core::ptr::from_ref(msg).cast::<c_void>();
        // SAFETY: `idx < effective_max <= max_count` and `effective_max * ptr_size <= isize::MAX`,
        // so `out_buf.add(idx * ptr_size)` stays within `out_buf`'s `max_count * ptr_size`
        // writable byte allocation without `isize` offset overflow; `write_unaligned` preserves
        // full pointer provenance regardless of `out_buf` alignment.
        unsafe {
            core::ptr::write_unaligned(
                out_buf.add(idx * ptr_size).cast::<*const c_void>(),
                msg_ptr,
            );
        }
    });
    // SAFETY: `out_count` is non-null and writable.
    unsafe {
        *out_count = written;
    }
    FfiResult::OK
}

fn read_u32_framed_slice<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<&'a [u8], FfiResult> {
    let end_len = pos.checked_add(4).ok_or(FfiResult::INVALID_ARGUMENT)?;
    let len_hdr = bytes
        .get(*pos..end_len)
        .ok_or(FfiResult::INVALID_ARGUMENT)?;
    let len = u32::from_le_bytes([len_hdr[0], len_hdr[1], len_hdr[2], len_hdr[3]]) as usize;
    *pos = end_len;
    let end_payload = pos.checked_add(len).ok_or(FfiResult::INVALID_ARGUMENT)?;
    let payload = bytes
        .get(*pos..end_payload)
        .ok_or(FfiResult::INVALID_ARGUMENT)?;
    *pos = end_payload;
    Ok(payload)
}

/// # Safety
///
/// - `world` must grant exclusive write access to `msg_res_id` (`Messages<M>`).
/// - `data_ptr` must be null or readable for `data_len` bytes (`>= 4`).
unsafe fn msg_send_non_pod_impl<M: Message + ModComponent + Default>(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    if data_len < 4 {
        return FfiResult::INVALID_ARGUMENT;
    }
    if data_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(sym) = M::symbols().iter().find(|s| {
        s.name
            .strip_prefix("bam.v0.")
            .and_then(|rest| rest.strip_suffix(".import_field"))
            == Some(M::CANONICAL_NAME)
    }) else {
        return FfiResult::UNSUPPORTED;
    };
    if sym.ptr.is_null() {
        return FfiResult::UNSUPPORTED;
    }
    type ImportFieldFn = unsafe extern "C" fn(
        dst_ptr: *mut c_void,
        field_name: *const StrView,
        type_tag: *const StrView,
        payload: *const FfiSlice<u8>,
    ) -> FfiResult;
    // SAFETY: Witness codegen exports `"bam.v0.<M>.import_field"` with `ImportFieldFn` signature.
    let import_fn: ImportFieldFn = unsafe { core::mem::transmute(sym.ptr) };

    // SAFETY: Caller guarantees `data_ptr` is readable for `data_len` bytes (`>= 4`).
    let bytes = unsafe { core::slice::from_raw_parts(data_ptr, data_len) };
    let field_count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let mut pos = 4usize;
    let mut host_msg = M::default();

    for _ in 0..field_count {
        let name_bytes = match read_u32_framed_slice(bytes, &mut pos) {
            Ok(s) => s,
            Err(err) => return err,
        };
        let Ok(name_str) = core::str::from_utf8(name_bytes) else {
            return FfiResult::INVALID_UTF8;
        };
        let tag_bytes = match read_u32_framed_slice(bytes, &mut pos) {
            Ok(s) => s,
            Err(err) => return err,
        };
        let Ok(tag_str) = core::str::from_utf8(tag_bytes) else {
            return FfiResult::INVALID_UTF8;
        };
        let val_bytes = match read_u32_framed_slice(bytes, &mut pos) {
            Ok(s) => s,
            Err(err) => return err,
        };

        let name_view = StrView::from_str(name_str);
        let tag_view = StrView::from_str(tag_str);
        let payload_slice = FfiSlice::from_slice(val_bytes);
        // SAFETY: `&raw mut host_msg` points to an initialized `M`, and `name_view`, `tag_view`,
        // and `payload_slice` borrow valid UTF-8 / byte slices on stack for the synchronous call.
        let status = unsafe {
            import_fn(
                (&raw mut host_msg).cast::<c_void>(),
                &raw const name_view,
                &raw const tag_view,
                &raw const payload_slice,
            )
        };
        if !status.is_ok() {
            return status;
        }
    }

    if pos != bytes.len() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Caller declared `MESSAGE_WRITE` on `msg_res_id`.
    unsafe { push_message_to_world(world, last_run, this_run, msg_res_id, host_msg) }
}

/// Monomorphized typed component insertion function using unaligned memory reading.
///
/// # Safety
/// Caller must ensure `T` has no drop glue or heap pointers and `data` points to `size_of::<T>()`
/// readable bytes representing a valid bit-pattern for `T` (padding bytes may be uninitialized).
unsafe fn insert_raw_impl<T: Component + ModComponent>(
    world: &mut World,
    entity: Entity,
    data: *const u8,
) {
    if core::mem::size_of::<T>() > 0 && data.is_null() {
        return;
    }
    let comp: T = if core::mem::size_of::<T>() == 0 {
        // SAFETY: `T` is zero-sized and `T::IS_RAW_INSERTABLE && !needs_drop::<T>()` holds.
        unsafe { core::mem::MaybeUninit::<T>::zeroed().assume_init() }
    } else {
        // SAFETY: `data` is non-null and holds `size_of::<T>()` bytes for `T`; `read_unaligned`
        // copies `T` directly without forming `&[u8]` over padding bytes.
        unsafe { core::ptr::read_unaligned(data.cast::<T>()) }
    };
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(comp);
    }
}

/// # Safety
///
/// - `world` must grant exclusive write access to `msg_res_id` (`Messages<M>`).
/// - If `count > 0` and `size_of::<M>() > 0`, `data_ptr` must be readable for
///   `count * size_of::<M>()` bytes.
unsafe fn msg_write_batch_impl<M: Message + AbiPod>(
    world: UnsafeWorldCell<'_>,
    last_run: Tick,
    this_run: Tick,
    msg_res_id: ComponentId,
    data_ptr: *const u8,
    count: usize,
) -> FfiResult {
    if count == 0 {
        return FfiResult::OK;
    }
    if core::mem::size_of::<M>() > 0 && data_ptr.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: `MESSAGE_WRITE` on `msg_res_id` (`Messages<M>`) was declared in `FilteredAccessSet`.
    let Some(entity) = (unsafe { world.resource_entities() }).get(msg_res_id) else {
        return FfiResult::NOT_FOUND;
    };
    let Ok(entity_cell) = world.get_entity_with_ticks(entity, last_run, this_run) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: Exclusive mutable access to `Messages<M>` is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(msg_res_id) }) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: `msg_res_id` was registered for `Messages<M>`.
    let mut messages = unsafe { mut_untyped.with_type::<Messages<M>>() };
    let msg_size = core::mem::size_of::<M>();
    if msg_size == 0 {
        for _ in 0..count {
            // SAFETY: `M: AbiPod` is zero-sized, `Copy`, and valid when zeroed.
            let zst = unsafe { core::mem::MaybeUninit::<M>::zeroed().assume_init() };
            messages.write(zst);
        }
    } else {
        for i in 0..count {
            // SAFETY: Caller verified `data_ptr` is readable for `count * size_of::<M>()` bytes.
            let msg = unsafe { core::ptr::read_unaligned(data_ptr.cast::<M>().add(i)) };
            messages.write(msg);
        }
    }
    FfiResult::OK
}

/// # Safety
///
/// - `data` must be null or readable for 8 bytes encoding an `FfiChildOf` (`FfiEntity` parent handle).
unsafe fn insert_child_of_raw(world: &mut World, entity: Entity, data: *const u8) {
    if data.is_null() {
        return;
    }
    // SAFETY: `data` points to an 8-byte `FfiChildOf` payload (`FfiEntity` parent handle).
    let raw_bits = unsafe { core::ptr::read_unaligned(data.cast::<u64>()) };
    let Some(parent) = Entity::try_from_bits(raw_bits) else {
        return;
    };
    if parent == entity || world.get_entity(parent).is_err() {
        return;
    }
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(ChildOf(parent));
    }
}

/// # Safety
///
/// - When `size_of::<E>() > 0`, `data` must be null or readable for `size_of::<E>()` bytes encoding a valid `E`.
unsafe fn trigger_event_impl<E>(
    world: &mut World,
    _target: Option<Entity>,
    data: *const u8,
) -> Result<(), FfiResult>
where
    E: Event<Trigger<'static>: Default> + ModComponent + Copy,
{
    if core::mem::size_of::<E>() > 0 && data.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    let event: E = if core::mem::size_of::<E>() == 0 {
        // SAFETY: `E` is zero-sized and valid as POD event data.
        unsafe { core::mem::MaybeUninit::<E>::zeroed().assume_init() }
    } else {
        // SAFETY: `data` points to `size_of::<E>()` initialized bytes.
        unsafe { core::ptr::read_unaligned(data.cast::<E>()) }
    };
    world.trigger(event);
    Ok(())
}

/// # Safety
///
/// - When `size_of::<E>() > 0`, `data` must be null or readable for `size_of::<E>()` bytes encoding a valid `E`.
unsafe fn trigger_entity_event_impl<E>(
    world: &mut World,
    target: Option<Entity>,
    data: *const u8,
) -> Result<(), FfiResult>
where
    E: EntityEvent + Event + ModComponent + Copy,
    for<'a> E::Trigger<'a>: Default,
{
    if core::mem::size_of::<E>() > 0 && data.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    // SAFETY: `data` points to `size_of::<E>()` initialized bytes.
    let event: E = unsafe { core::ptr::read_unaligned(data.cast::<E>()) };
    let target_entity = target.unwrap_or_else(|| event.event_target());
    if !world.entities().contains(target_entity) {
        return Err(FfiResult::INVALID_ENTITY);
    }
    world.trigger(event);
    Ok(())
}

/// # Safety
///
/// - When `size_of::<E>() > 0`, `data` must be null or readable for `size_of::<E>()` bytes encoding a valid `E`.
unsafe fn extract_entity_event_target_impl<E: EntityEvent + Copy>(
    data: *const u8,
) -> Option<Entity> {
    if core::mem::size_of::<E>() > 0 && data.is_null() {
        return None;
    }
    // SAFETY: `data` points to an initialized `E` value during observer execution.
    let event: E = unsafe { core::ptr::read_unaligned(data.cast::<E>()) };
    Some(event.event_target())
}

/// Monomorphized host-side `T::default()` component constructor and inserter.
fn insert_default_impl<T: Component + Default>(world: &mut World, entity: Entity) {
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(T::default());
    }
}

/// Fluent registration options configuring how a host Rust [`Component`] is exposed to guest mods.
///
/// By default (`ComponentOptions::default()`), raw byte insertion (`cmd_insert` / `cmd_spawn`) is
/// enabled automatically when `T::IS_RAW_INSERTABLE && !needs_drop::<T>()`, and disabled otherwise.
/// Call [`.opaque()`](Self::opaque) to force VTable-only access even for `RawInsertable` types, or
/// [`.with_default()`](Self::with_default) to expose host-constructed `T::default()` via `cmd_insert_default`.
pub struct ComponentOptions<T> {
    allow_raw_insert: bool,
    insert_default_fn: Option<InsertDefaultFn>,
    extra_storage_flags: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for ComponentOptions<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for ComponentOptions<T> {}

impl<T> core::fmt::Debug for ComponentOptions<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ComponentOptions")
            .field("allow_raw_insert", &self.allow_raw_insert)
            .field("has_default", &self.insert_default_fn.is_some())
            .field("extra_storage_flags", &self.extra_storage_flags)
            .finish()
    }
}

impl<T: Component + ModComponent> Default for ComponentOptions<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Component + ModComponent> ComponentOptions<T> {
    /// Creates default options for `T`, automatically enabling raw byte insertion iff
    /// `T::IS_RAW_INSERTABLE && !core::mem::needs_drop::<T>()` and inheriting Bevy `0.20`
    /// `T::HAS_SUMMARY_TICK` into `BAM_STORAGE_FLAG_SUMMARY_TICK`.
    #[inline]
    pub const fn new() -> Self {
        let mut extra_storage_flags = component_flags_to_storage_flags(T::COMPONENT_FLAGS);
        if T::HAS_SUMMARY_TICK {
            extra_storage_flags |= BAM_STORAGE_FLAG_SUMMARY_TICK;
        }
        Self {
            allow_raw_insert: T::IS_RAW_INSERTABLE && !core::mem::needs_drop::<T>(),
            insert_default_fn: None,
            extra_storage_flags,
            _marker: PhantomData,
        }
    }

    /// Disables raw byte insertion (`cmd_insert` and `cmd_spawn` will return
    /// [`bamabi_sys::FfiResult::PERMISSION_DENIED`]), restricting guest mutations to VTable
    /// methods or [`Self::with_default`].
    #[inline]
    pub const fn opaque(mut self) -> Self {
        self.allow_raw_insert = false;
        self
    }

    /// Explicitly enables raw byte insertion for `T: RawInsertable`.
    #[inline]
    pub const fn with_raw_insert(mut self) -> Self
    where
        T: RawInsertable,
    {
        self.allow_raw_insert = !core::mem::needs_drop::<T>();
        self
    }

    /// Exposes host-side `T::default()` construction for `cmd_insert_default`.
    #[inline]
    pub fn with_default(mut self) -> Self
    where
        T: Default,
    {
        self.insert_default_fn = Some(insert_default_impl::<T>);
        self
    }

    /// Marks this component registration as immutable (`BAM_STORAGE_FLAG_IMMUTABLE`), rejecting
    /// guest `WRITE` and `OPTIONAL_WRITE` query access with `PERMISSION_DENIED`.
    #[inline]
    pub const fn immutable(mut self) -> Self {
        self.extra_storage_flags |= BAM_STORAGE_FLAG_IMMUTABLE;
        self
    }

    /// Marks this component registration as sparse-set stored (`BAM_STORAGE_FLAG_SPARSE_SET`).
    #[inline]
    pub const fn sparse_set(mut self) -> Self {
        self.extra_storage_flags |= BAM_STORAGE_FLAG_SPARSE_SET;
        self
    }

    /// Marks this component registration as carrying a Bevy `0.20` per-table summary tick
    /// (`BAM_STORAGE_FLAG_SUMMARY_TICK`).
    #[inline]
    pub const fn with_summary_tick(mut self) -> Self {
        self.extra_storage_flags |= BAM_STORAGE_FLAG_SUMMARY_TICK;
        self
    }

    /// Alias for [`Self::with_summary_tick`] retained for backwards compatibility.
    #[inline]
    pub const fn summary_tick(self) -> Self {
        self.with_summary_tick()
    }

    /// Returns additional `BAM_STORAGE_FLAG_*` bits configured on these options.
    #[inline]
    pub const fn extra_storage_flags(&self) -> u32 {
        self.extra_storage_flags
    }
}

#[inline]
pub(crate) const fn component_flags_to_storage_flags(component_flags: u32) -> u32 {
    let mut out = 0u32;
    if (component_flags & BAM_COMPONENT_FLAG_SPARSE_SET) != 0 {
        out |= BAM_STORAGE_FLAG_SPARSE_SET;
    }
    if (component_flags & BAM_COMPONENT_FLAG_IMMUTABLE) != 0 {
        out |= BAM_STORAGE_FLAG_IMMUTABLE;
    }
    if (component_flags & BAM_COMPONENT_FLAG_SUMMARY_TICK) != 0 {
        out |= BAM_STORAGE_FLAG_SUMMARY_TICK;
    }
    out
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SymbolRecord {
    pub(crate) addr: SymbolAddress,
    pub(crate) owner: Option<ModId>,
}

/// Retained heap strings backing dynamic [`BamFieldDesc::name`] and [`BamFieldDesc::type_tag`].
pub type OwnedFieldStrings = Arc<[(Arc<str>, Arc<str>)]>;

/// Extracts ABI `0.3` `schema_fingerprint` and host-retained [`BamFieldDesc`] metadata from a
/// dynamic [`ComponentDesc`] when covered by `desc.struct_size`.
///
/// # Safety
/// - If `desc` covers `field_count` and `desc.field_count > 0`, `desc.fields` must point to
///   `desc.field_count` readable [`BamFieldDesc`] entries whose non-empty [`StrView`]s point to
///   initialized bytes.
pub(crate) unsafe fn extract_owned_dynamic_fields(
    desc: &ComponentDesc,
) -> Result<(u64, Arc<[BamFieldDesc]>, Option<OwnedFieldStrings>), (FfiResult, String)> {
    let schema_fingerprint = if desc.covers_slot(
        core::mem::offset_of!(ComponentDesc, schema_fingerprint),
        core::mem::size_of::<u64>(),
    ) {
        desc.schema_fingerprint
    } else {
        0
    };

    let has_fields = desc.covers_slot(
        core::mem::offset_of!(ComponentDesc, field_count),
        core::mem::size_of::<usize>(),
    ) && desc.field_count > 0;
    if !has_fields {
        return Ok((schema_fingerprint, Arc::from([]), None));
    }
    if desc.fields.is_null() {
        return Err((
            FfiResult::NULL_POINTER,
            "ComponentDesc `fields` pointer is null with non-zero `field_count`".to_string(),
        ));
    }

    // SAFETY: Caller guarantees `desc.fields` is readable for `desc.field_count` entries.
    let raw_fields = unsafe { core::slice::from_raw_parts(desc.fields, desc.field_count) };
    let mut string_pairs: Vec<(Arc<str>, Arc<str>)> = Vec::with_capacity(raw_fields.len());
    let mut field_descs: Vec<BamFieldDesc> = Vec::with_capacity(raw_fields.len());

    for (idx, field) in raw_fields.iter().enumerate() {
        if (field.struct_size as usize) < BamFieldDesc::MIN_V0_3_SIZE {
            return Err((
                FfiResult::INVALID_VERSION,
                format!(
                    "BamFieldDesc[{idx}] struct_size ({}) < MIN_V0_3_SIZE ({})",
                    field.struct_size,
                    BamFieldDesc::MIN_V0_3_SIZE
                ),
            ));
        }
        // SAFETY: Caller guarantees each field `name` points to readable bytes when `len > 0`.
        let name_str = unsafe { field.name.try_as_str() }.map_err(|err| {
            (
                err,
                format!("BamFieldDesc[{idx}] `name` StrView validation failed ({err})"),
            )
        })?;
        // SAFETY: Caller guarantees each field `type_tag` points to readable bytes when `len > 0`.
        let tag_str = unsafe { field.type_tag.try_as_str() }.map_err(|err| {
            (
                err,
                format!("BamFieldDesc[{idx}] `type_tag` StrView validation failed ({err})"),
            )
        })?;
        string_pairs.push((Arc::from(name_str), Arc::from(tag_str)));
    }

    let owned_strings: OwnedFieldStrings = Arc::from(string_pairs);
    for (field, (name_arc, tag_arc)) in raw_fields.iter().zip(owned_strings.iter()) {
        field_descs.push(BamFieldDesc::new(
            field.kind,
            StrView::from_str(name_arc),
            StrView::from_str(tag_arc),
            field.offset,
            field.size,
            field.align,
        ));
    }
    Ok((
        schema_fingerprint,
        Arc::from(field_descs),
        Some(owned_strings),
    ))
}

/// Catalog record stored for each component or singleton resource in [`RegistrySnapshot`].
#[derive(Clone, Debug)]
pub struct RegisteredComponent {
    /// 1-based [`BamComponent`] handle (`index == (handle.0 - 1) as usize`).
    pub handle: BamComponent,
    /// Canonical namespaced component or resource name (e.g. `"my_game::Position"`).
    pub name: Arc<str>,
    /// Deterministic 128-bit UUID.
    pub uuid: ComponentUuid,
    /// Runtime Bevy [`ComponentId`].
    pub component_id: ComponentId,
    /// Byte size (`size_of::<T>()`).
    pub size: usize,
    /// Byte alignment (`align_of::<T>()`).
    pub align: usize,
    /// Deterministic 64-bit FNV-1a schema fingerprint (`ModComponent::SCHEMA_FINGERPRINT`).
    pub schema_fingerprint: u64,
    /// Host-retained ABI `0.3` compile-time field reflection metadata (`ModComponent::BAM_FIELDS`).
    pub fields: Arc<[BamFieldDesc]>,
    /// Owned string backing for dynamic guest [`Self::fields`] entries (`None` for `'static` host entries).
    pub owned_field_strings: Option<OwnedFieldStrings>,
    /// Bitmask of `BAM_STORAGE_FLAG_*` (`SPARSE_SET`, `IMMUTABLE`, `RESOURCE`).
    pub storage_flags: u32,
    /// Whether this type satisfies cross-language `AbiPod`.
    pub is_abi_pod: bool,
    /// Whether this component was registered dynamically by a guest mod at runtime.
    pub is_dynamic: bool,
    /// Address of the C-ABI VTable (`SymbolAddress::NULL` if none or evicted on unload).
    pub vtable_ptr: SymbolAddress,
    /// Monomorphized raw byte inserter (`None` for dynamic, opaque, or resource entries).
    pub insert_raw_fn: Option<InsertRawFn>,
    /// Optional host-side `T::default()` constructor for `cmd_insert_default`.
    pub insert_default_fn: Option<InsertDefaultFn>,
    /// Optional foreign drop callback (cleared on guest unload).
    pub drop_fn: Option<ComponentDropFn>,
    /// Optional drop trampoline slot index assigned in [`crate::WorldDropGuard`].
    pub drop_slot: Option<usize>,
    /// Whether `on_add` hook trampoline was registered on Bevy's `ComponentHooks`.
    pub has_on_add_hook: bool,
    /// Whether `on_remove` hook trampoline was registered on Bevy's `ComponentHooks`.
    pub has_on_remove_hook: bool,
    /// Optional [`ModId`] owning a dynamic component's VTable and drop callback.
    pub create_msg_cursor_fn: Option<CreateMsgCursorFn>,
    /// Optional monomorphized batch reader for `Messages<M>`.
    pub msg_read_batch_fn: Option<MsgReadBatchFn>,
    /// Optional monomorphized writer for `Messages<M>`.
    pub msg_send_fn: Option<MsgSendFn>,
    /// Optional monomorphized batch writer for `Messages<M>`.
    pub msg_write_batch_fn: Option<MsgWriteBatchFn>,
    /// Whether raw 8-byte entity payloads (such as `ChildOf`) must be validated before insert.
    pub validates_entity_payload: bool,
    /// Whether `guest_observer_runner` synthesizes an aligned stack [`FfiEntity`] wire payload
    /// instead of forwarding raw `&E` bytes (used for bridged `#[repr(Rust)]` entity events like `bevy_scene::Ready`).
    pub synthesizes_ffi_entity_payload: bool,
    /// Bevy [`EventKey`] when registered as a triggerable host [`Event`] / [`EntityEvent`] or dynamic event.
    pub event_key: Option<EventKey>,
    /// Optional safe entity-only trigger constructor (`fn(&mut World, Entity)`) for bridged
    /// `EntityEvent` types such as `bevy_scene::Ready` that do not implement `From<Entity>`.
    pub entity_event_trigger_fn: Option<EntityEventTriggerFn>,
    /// Monomorphized typed `world.trigger(...)` dispatcher for host-registered events.
    pub trigger_event_fn: Option<TriggerEventFn>,
    /// Extractor returning the target entity from raw `EntityEvent` payload bytes.
    pub extract_event_target_fn: Option<ExtractEventTargetFn>,
    /// Optional extractor reading `RelationshipTarget` entities (such as `Children`) into an [`FfiEntity`] slice.
    pub extract_relationship_targets_fn: Option<ExtractRelationshipTargetsFn>,
    /// Optional [`ModId`] owning a dynamic component's VTable and drop callback.
    pub owner: Option<ModId>,
}

impl RegisteredComponent {
    /// Returns `true` if this entry represents a singleton Bevy `Resource`.
    #[inline]
    pub const fn is_resource(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_RESOURCE) != 0
    }

    /// Returns `true` if this entry represents a buffered Bevy `Messages<M>` stream.
    #[inline]
    pub const fn is_message(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_MESSAGE) != 0
    }

    /// Returns `true` if this entry represents a registered triggerable Bevy `Event`.
    #[inline]
    pub const fn is_event(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_EVENT) != 0
    }

    /// Returns `true` if this entry represents an entity-targeted Bevy `EntityEvent`.
    #[inline]
    pub const fn is_entity_event(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_ENTITY_EVENT) != 0
    }

    /// Returns `true` if this entry uses `StorageType::SparseSet`.
    #[inline]
    pub const fn is_sparse_set(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_SPARSE_SET) != 0
    }

    /// Returns `true` if mutable guest access (`WRITE`, `OPTIONAL_WRITE`, `RESOURCE_WRITE`) is forbidden.
    #[inline]
    pub const fn is_immutable(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_IMMUTABLE) != 0
    }

    /// Returns `true` if `BAM_STORAGE_FLAG_SUMMARY_TICK` is set.
    #[inline]
    pub const fn is_summary_tick(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_SUMMARY_TICK) != 0
    }
}

/// Catalog record for a typed entity relationship registered via `host_register_relation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredRelation {
    /// 1-based [`BamComponent`] handle representing this relationship.
    pub handle: BamComponent,
    /// Canonical namespaced relation name (e.g. `"combat::TargetOf"`).
    pub name: Arc<str>,
    /// Deterministic 128-bit UUID identifying the relation.
    pub uuid: ComponentUuid,
    /// Bevy [`ComponentId`] of the per-relation sparse-set marker component holding
    /// `on_despawn` and `on_remove` lifecycle hooks.
    pub component_id: ComponentId,
    /// Bitmask of `BAM_RELATION_FLAG_*` (`EXCLUSIVE`, `CASCADE_DESPAWN`/`CASCADE_DELETE`,
    /// `SYMMETRIC`, `ACYCLIC`, `CLEANUP_REMOVE`).
    pub flags: u32,
    /// Edge payload byte size (`0` for zero-sized marker/tag edges).
    pub data_size: usize,
    /// Edge payload byte alignment (`>= 1`, power of two).
    pub data_align: usize,
    /// Optional out-degree cap (`0` = unlimited; `1` behaves identically to `EXCLUSIVE`).
    pub max_degree: u32,
}

impl RegisteredRelation {
    /// Returns `true` if each source entity may hold at most one outgoing edge for this relation
    /// (`BAM_RELATION_FLAG_EXCLUSIVE` or `max_degree == 1`), replacing prior targets on insert.
    #[inline]
    pub const fn is_exclusive(&self) -> bool {
        (self.flags & BAM_RELATION_FLAG_EXCLUSIVE) != 0 || self.max_degree == 1
    }

    /// Returns `true` if despawning a target entity cascades despawn to dependent source entities
    /// (`BAM_RELATION_FLAG_CASCADE_DESPAWN` / `BAM_RELATION_FLAG_CASCADE_DELETE`).
    #[inline]
    pub const fn is_cascade_despawn(&self) -> bool {
        (self.flags & BAM_RELATION_FLAG_CASCADE_DESPAWN) != 0
    }

    /// Returns `true` if edge `(A -> B)` atomically maintains reciprocal `(B -> A)`
    /// (`BAM_RELATION_FLAG_SYMMETRIC`).
    #[inline]
    pub const fn is_symmetric(&self) -> bool {
        (self.flags & BAM_RELATION_FLAG_SYMMETRIC) != 0
    }

    /// Returns `true` if directed cycles (including self-loops) are rejected with
    /// [`FfiResult::CYCLE_DETECTED`] (`BAM_RELATION_FLAG_ACYCLIC`).
    #[inline]
    pub const fn is_acyclic(&self) -> bool {
        (self.flags & BAM_RELATION_FLAG_ACYCLIC) != 0
    }

    /// Returns `true` if self-referential `(entity -> entity)` edges are permitted (all
    /// non-`ACYCLIC` relations, aligning with Bevy `0.19`/`0.20` self-referential relations).
    #[inline]
    pub const fn allows_self_referential(&self) -> bool {
        !self.is_acyclic()
    }

    /// Returns `true` if `BAM_RELATION_FLAG_CLEANUP_REMOVE` is set.
    #[inline]
    pub const fn is_cleanup_remove(&self) -> bool {
        (self.flags & BAM_RELATION_FLAG_CLEANUP_REMOVE) != 0
    }

    /// Returns the effective per-entity outgoing degree cap, or `None` when unlimited.
    #[inline]
    pub const fn effective_max_degree(&self) -> Option<usize> {
        if self.is_exclusive() {
            Some(1)
        } else if self.max_degree > 0 {
            Some(self.max_degree as usize)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExtendedDynamicFlags {
    pub(crate) has_on_add_effective: bool,
    pub(crate) has_on_insert: bool,
    pub(crate) has_on_discard: bool,
    pub(crate) has_on_remove: bool,
    pub(crate) has_on_despawn: bool,
    pub(crate) has_clone_fn: bool,
}

/// Core layout & identity fields shared when inserting a fresh [`RegisteredComponent`].
pub(crate) struct NewComponentBase<'a> {
    pub(crate) name: &'a str,
    pub(crate) uuid: ComponentUuid,
    pub(crate) component_id: ComponentId,
    pub(crate) size: usize,
    pub(crate) align: usize,
    pub(crate) storage_flags: u32,
    pub(crate) is_abi_pod: bool,
}

/// Bundled specification for registering a dynamic guest component into [`ModRegistry`].
pub(crate) struct DynamicComponentReg<'a> {
    pub(crate) comp_id: ComponentId,
    pub(crate) desc: &'a ComponentDesc,
    pub(crate) name: &'a str,
    pub(crate) owner: Option<ModId>,
    pub(crate) drop_slot: Option<usize>,
    pub(crate) required_components: Box<[BamComponent]>,
    pub(crate) schema_fingerprint: u64,
    pub(crate) fields: Arc<[BamFieldDesc]>,
    pub(crate) owned_field_strings: Option<OwnedFieldStrings>,
}

/// Bundled specification for registering a dynamic guest singleton resource into [`ModRegistry`].
pub(crate) struct DynamicResourceReg<'a> {
    pub(crate) comp_id: ComponentId,
    pub(crate) desc: &'a ComponentDesc,
    pub(crate) name: &'a str,
    pub(crate) owner: Option<ModId>,
    pub(crate) drop_slot: Option<usize>,
    pub(crate) schema_fingerprint: u64,
    pub(crate) fields: Arc<[BamFieldDesc]>,
    pub(crate) owned_field_strings: Option<OwnedFieldStrings>,
}

/// Bundled specification for registering a dynamic guest buffered message channel into [`ModRegistry`].
pub(crate) struct DynamicMessageReg<'a> {
    pub(crate) desc: &'a ComponentDesc,
    pub(crate) name: &'a str,
    pub(crate) owner: Option<ModId>,
    pub(crate) is_active: Arc<AtomicBool>,
    pub(crate) schema_fingerprint: u64,
    pub(crate) fields: Arc<[BamFieldDesc]>,
    pub(crate) owned_field_strings: Option<OwnedFieldStrings>,
}

/// Bundled specification for registering a typed relationship entry into [`ModRegistry`].
pub(crate) struct RelationRegSpec<'a> {
    pub(crate) comp_id: ComponentId,
    pub(crate) name: &'a str,
    pub(crate) uuid: ComponentUuid,
    pub(crate) flags: u32,
    pub(crate) data_size: usize,
    pub(crate) data_align: usize,
    pub(crate) max_degree: u32,
    pub(crate) owner: Option<ModId>,
}

impl RegisteredComponent {
    pub(crate) fn new_base(
        handle: BamComponent,
        name: Arc<str>,
        base: &NewComponentBase<'_>,
    ) -> Self {
        Self {
            handle,
            name,
            uuid: base.uuid,
            component_id: base.component_id,
            size: base.size,
            align: base.align,
            schema_fingerprint: 0,
            fields: Arc::from([]),
            owned_field_strings: None,
            storage_flags: base.storage_flags,
            is_abi_pod: base.is_abi_pod,
            is_dynamic: false,
            vtable_ptr: SymbolAddress::NULL,
            insert_raw_fn: None,
            insert_default_fn: None,
            drop_fn: None,
            drop_slot: None,
            has_on_add_hook: false,
            has_on_remove_hook: false,
            create_msg_cursor_fn: None,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
            validates_entity_payload: false,
            synthesizes_ffi_entity_payload: false,
            event_key: None,
            entity_event_trigger_fn: None,
            trigger_event_fn: None,
            extract_event_target_fn: None,
            extract_relationship_targets_fn: None,
            owner: None,
        }
    }
}

/// Immutable dense snapshot shared via `Arc` across `ModRegistry`, `HostContext`, and `GuestSystem`.
#[derive(Clone, Default)]
pub struct RegistrySnapshot {
    pub(crate) entries: Vec<RegisteredComponent>,
    pub(crate) by_name: HashMap<Arc<str>, BamComponent>,
    pub(crate) by_uuid: HashMap<ComponentUuid, BamComponent>,
    pub(crate) by_component_id: HashMap<ComponentId, BamComponent>,
    pub(crate) symbols: HashMap<Arc<str>, SymbolRecord>,
    pub(crate) required_components: HashMap<BamComponent, Box<[BamComponent]>>,
    pub(crate) extended_hook_flags: HashMap<BamComponent, ExtendedDynamicFlags>,
    pub(crate) relations: HashMap<BamComponent, RegisteredRelation>,
    asset_backend: Option<Arc<dyn HostAssetBackend>>,
    scene_backend: Option<Arc<dyn HostSceneBackend>>,
}

impl RegistrySnapshot {
    /// Allocates the next 1-based [`BamComponent`] handle, constructs a base
    /// [`RegisteredComponent`], runs `configure`, and inserts it into all 4 lookup indices.
    pub(crate) fn alloc_handle_and_insert(
        &mut self,
        base: NewComponentBase<'_>,
        configure: impl FnOnce(&mut RegisteredComponent),
    ) -> Result<BamComponent, ComponentCollisionError> {
        let next_raw = u32::try_from(self.entries.len() + 1).map_err(|_| {
            ComponentCollisionError::MetadataMismatch {
                name: base.name.to_owned(),
                reason: "ModRegistry exceeded u32::MAX component handles".to_owned(),
            }
        })?;
        let handle = BamComponent::from_raw(next_raw);
        let arc_name: Arc<str> = Arc::from(base.name);
        let mut entry = RegisteredComponent::new_base(handle, Arc::clone(&arc_name), &base);
        configure(&mut entry);
        self.entries.push(entry);
        self.by_name.insert(arc_name, handle);
        self.by_uuid.insert(base.uuid, handle);
        self.by_component_id.insert(base.component_id, handle);
        Ok(handle)
    }

    /// Looks up a component or resource in `O(1)` by its 1-based [`BamComponent`] handle.
    #[inline]
    pub fn get_by_handle(&self, handle: BamComponent) -> Option<&RegisteredComponent> {
        let idx = handle.0.checked_sub(1)? as usize;
        self.entries.get(idx)
    }

    /// Looks up a component or resource by its runtime Bevy [`ComponentId`].
    #[inline]
    pub fn get_by_id(&self, id: ComponentId) -> Option<&RegisteredComponent> {
        let &handle = self.by_component_id.get(&id)?;
        self.get_by_handle(handle)
    }

    /// Looks up a component or resource by canonical string name.
    #[inline]
    pub fn get_by_name(&self, name: &str) -> Option<&RegisteredComponent> {
        let &handle = self.by_name.get(name)?;
        self.get_by_handle(handle)
    }

    /// Looks up a component or resource by 128-bit [`ComponentUuid`].
    #[inline]
    pub fn get_by_uuid(&self, uuid: &ComponentUuid) -> Option<&RegisteredComponent> {
        let &handle = self.by_uuid.get(uuid)?;
        self.get_by_handle(handle)
    }

    /// Resolves a `"bam.v0.*"` symbol or VTable address in this snapshot.
    #[inline]
    pub fn resolve_symbol(&self, name: &str) -> Option<*const c_void> {
        if !name.starts_with("bam.v0.") {
            return None;
        }
        let rec = self.symbols.get(name)?;
        if rec.addr.is_null() {
            None
        } else {
            Some(rec.addr.as_ptr())
        }
    }

    /// Returns the total number of registered components and resources.
    #[inline]
    pub fn component_count(&self) -> usize {
        self.entries.len()
    }

    /// Returns the total number of registered `"bam.v0.*"` symbols in this snapshot.
    #[inline]
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    /// Returns the dense slice of all registered component/resource entries.
    #[inline]
    pub fn entries(&self) -> &[RegisteredComponent] {
        &self.entries
    }

    /// Borrows the optional [`HostAssetBackend`] registered on this snapshot.
    #[inline]
    pub fn asset_backend(&self) -> Option<&Arc<dyn HostAssetBackend>> {
        self.asset_backend.as_ref()
    }

    /// Borrows the optional [`HostSceneBackend`] registered on this snapshot.
    #[inline]
    pub fn scene_backend(&self) -> Option<&Arc<dyn HostSceneBackend>> {
        self.scene_backend.as_ref()
    }

    /// Returns the required [`BamComponent`] handles registered for `handle` (empty if none).
    #[inline]
    pub fn required_components(&self, handle: BamComponent) -> &[BamComponent] {
        self.required_components
            .get(&handle)
            .map_or(&[], |boxed| boxed.as_ref())
    }

    /// Looks up registered relationship metadata in `O(1)` by its 1-based [`BamComponent`] handle.
    #[inline]
    pub fn get_relation(&self, handle: BamComponent) -> Option<&RegisteredRelation> {
        self.relations.get(&handle)
    }

    /// Looks up registered relationship metadata by its runtime Bevy marker [`ComponentId`].
    #[inline]
    pub fn get_relation_by_id(&self, id: ComponentId) -> Option<&RegisteredRelation> {
        let &handle = self.by_component_id.get(&id)?;
        self.relations.get(&handle)
    }

    /// Looks up registered relationship metadata by deterministic 128-bit [`ComponentUuid`].
    #[inline]
    pub fn get_relation_by_uuid(&self, uuid: &ComponentUuid) -> Option<&RegisteredRelation> {
        let &handle = self.by_uuid.get(uuid)?;
        self.relations.get(&handle)
    }

    /// Returns `true` if `handle` refers to a relationship registered via `host_register_relation`.
    #[inline]
    pub fn is_relation(&self, handle: BamComponent) -> bool {
        self.relations.contains_key(&handle)
    }

    /// Returns the total number of registered relationships in this snapshot.
    #[inline]
    pub fn relation_count(&self) -> usize {
        self.relations.len()
    }
}

struct RegistryTracker {
    latest: ArcSwap<RegistrySnapshot>,
    generation: AtomicU64,
}

/// World-scoped Copy-on-Write (`CoW`) component, resource, and `"bam.v0.*"` symbol registry.
///
/// Cloning `ModRegistry` is an `O(1)` `Arc` bump. Mutations run on cold setup/load/unload paths via
/// `Arc::make_mut` and increment `generation` so `GuestSystem`s refresh their cached snapshot only
/// when the registry actually changes.
#[derive(Resource, Clone)]
pub struct ModRegistry {
    pub(super) snapshot: Arc<RegistrySnapshot>,
    generation: u64,
    tracker: Arc<RegistryTracker>,
}

impl Default for ModRegistry {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl ModRegistry {
    /// Creates an empty [`ModRegistry`] with **zero** built-in function symbols.
    #[inline]
    pub fn new() -> Self {
        let initial = Arc::new(RegistrySnapshot::default());
        let tracker = Arc::new(RegistryTracker {
            latest: ArcSwap::from(Arc::clone(&initial)),
            generation: AtomicU64::new(1),
        });
        Self {
            snapshot: initial,
            generation: 1,
            tracker,
        }
    }

    /// Returns the current monotonic mutation generation of this registry (`>= 1`).
    #[inline]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Reads the latest published generation across this registry's shared `World` lineage (`O(1)`).
    #[inline]
    pub fn latest_generation(&self) -> u64 {
        self.tracker.generation.load(Ordering::Acquire)
    }

    /// Loads the latest published `Arc<RegistrySnapshot>` across this registry's `World` lineage.
    #[inline]
    pub fn latest_snapshot_arc(&self) -> Arc<RegistrySnapshot> {
        self.tracker.latest.load_full()
    }

    /// Borrows the current [`RegistrySnapshot`].
    #[inline]
    pub fn snapshot(&self) -> &RegistrySnapshot {
        &self.snapshot
    }

    /// Clones the `Arc<RegistrySnapshot>` handle in `O(1)`.
    #[inline]
    pub fn snapshot_arc(&self) -> Arc<RegistrySnapshot> {
        Arc::clone(&self.snapshot)
    }

    /// Restores a previously captured `Arc<RegistrySnapshot>` (used by transactional load rollback)
    /// and advances `generation`.
    pub fn restore_snapshot(&mut self, snapshot: Arc<RegistrySnapshot>) {
        self.snapshot = Arc::clone(&snapshot);
        let next_gen = self
            .tracker
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.generation = next_gen;
        self.tracker.latest.store(snapshot);
    }

    #[inline]
    pub(crate) fn publish_after_mutation(&mut self) {
        self.tracker.latest.store(Arc::clone(&self.snapshot));
        let next_gen = self
            .tracker
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.generation = next_gen;
    }

    /// Installs or replaces the world-scoped [`HostAssetBackend`] servicing `asset_*` FFI slots.
    pub fn set_asset_backend(&mut self, backend: Arc<dyn HostAssetBackend>) {
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.asset_backend = Some(backend);
        self.publish_after_mutation();
    }

    /// Borrows the installed [`HostAssetBackend`], if any.
    #[inline]
    pub fn asset_backend(&self) -> Option<&Arc<dyn HostAssetBackend>> {
        self.snapshot.asset_backend()
    }

    /// Installs or replaces the world-scoped [`HostSceneBackend`] servicing `cmd_spawn_scene`.
    pub fn set_scene_backend(&mut self, backend: Arc<dyn HostSceneBackend>) {
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.scene_backend = Some(backend);
        self.publish_after_mutation();
    }

    /// Borrows the installed [`HostSceneBackend`], if any.
    #[inline]
    pub fn scene_backend(&self) -> Option<&Arc<dyn HostSceneBackend>> {
        self.snapshot.scene_backend()
    }

    /// Validates that registering `(name, uuid, size, align, storage_flags, expected_comp_id)` does
    /// not collide with an incompatible existing component or resource entry.
    pub fn check_registration_collision(
        &self,
        name: &str,
        uuid: ComponentUuid,
        size: usize,
        align: usize,
        storage_flags: u32,
        expected_comp_id: Option<ComponentId>,
    ) -> Result<(), ComponentCollisionError> {
        if let Some(existing_by_uuid) = self.snapshot.get_by_uuid(&uuid)
            && existing_by_uuid.name.as_ref() != name
        {
            return Err(ComponentCollisionError::UuidCollision {
                existing_name: existing_by_uuid.name.to_string(),
                new_name: name.to_string(),
                uuid,
            });
        }

        if let Some(existing) = self.snapshot.get_by_name(name) {
            if self.snapshot.is_relation(existing.handle) {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: "name is already registered as a relationship".to_string(),
                });
            }
            if existing.uuid != uuid {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!("UUID mismatch (existing={:?}, new={uuid:?})", existing.uuid),
                });
            }
            if existing.size != size || existing.align != align {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "layout mismatch (existing=({}, {}), new=({}, {}))",
                        existing.size, existing.align, size, align
                    ),
                });
            }
            if existing.storage_flags != storage_flags {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "storage_flags mismatch (existing=0x{:x}, new=0x{:x})",
                        existing.storage_flags, storage_flags
                    ),
                });
            }
            if let Some(comp_id) = expected_comp_id
                && existing.component_id != comp_id
            {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "ComponentId mismatch (existing={:?}, new={:?})",
                        existing.component_id, comp_id
                    ),
                });
            }
        }

        Ok(())
    }

    /// Validates that dynamically re-registering `name` with `has_drop_fn` is compatible with the
    /// destructor configuration permanently bound in Bevy's `ComponentInfo`.
    pub fn check_dynamic_drop_compatibility(
        &self,
        name: &str,
        has_drop_fn: bool,
    ) -> Result<(), ComponentCollisionError> {
        if let Some(existing) = self.snapshot.get_by_name(name) {
            if !existing.is_dynamic && has_drop_fn {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: "cannot attach foreign drop_fn to host-owned component".to_string(),
                });
            }
            if existing.drop_slot.is_some() != has_drop_fn {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "drop_fn presence mismatch on re-registration (existing_has_drop={}, new_has_drop={has_drop_fn})",
                        existing.drop_slot.is_some()
                    ),
                });
            }
        }
        Ok(())
    }

    /// Validates that dynamically re-registering `name` with `(has_on_add, has_on_remove)` matches
    /// the lifecycle hooks permanently wired into Bevy's `ComponentHooks`.
    pub fn check_dynamic_hook_compatibility(
        &self,
        name: &str,
        has_on_add: bool,
        has_on_remove: bool,
    ) -> Result<(), ComponentCollisionError> {
        if let Some(existing) = self.snapshot.get_by_name(name) {
            if !existing.is_dynamic && (has_on_add || has_on_remove) {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: "cannot attach foreign on_add/on_remove hooks to host-owned component"
                        .to_string(),
                });
            }
            if existing.has_on_add_hook != has_on_add
                || existing.has_on_remove_hook != has_on_remove
            {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "hook presence mismatch on re-registration (existing=({}, {}), new=({has_on_add}, {has_on_remove}))",
                        existing.has_on_add_hook, existing.has_on_remove_hook
                    ),
                });
            }
        }
        Ok(())
    }

    /// Validates that dynamically re-registering `name` matches all extended lifecycle hooks,
    /// `clone_fn`, and `required_components` presence permanently wired into Bevy's `ComponentInfo`.
    pub fn check_dynamic_extended_hook_compatibility(
        &self,
        name: &str,
        desc: &ComponentDesc,
    ) -> Result<(), ComponentCollisionError> {
        let new_flags = ExtendedDynamicFlags {
            has_on_add_effective: desc.on_add.is_some() || desc.required_component_count > 0,
            has_on_insert: desc.on_insert.is_some(),
            has_on_discard: desc.on_discard.is_some(),
            has_on_remove: desc.on_remove.is_some(),
            has_on_despawn: desc.on_despawn.is_some(),
            has_clone_fn: desc.clone_fn.is_some(),
        };
        if let Some(existing) = self.snapshot.get_by_name(name) {
            if !existing.is_dynamic
                && (new_flags.has_on_add_effective
                    || new_flags.has_on_insert
                    || new_flags.has_on_discard
                    || new_flags.has_on_remove
                    || new_flags.has_on_despawn
                    || new_flags.has_clone_fn)
            {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: "cannot attach foreign hooks/clone/required_components to host-owned component"
                        .to_string(),
                });
            }
            let existing_flags = self
                .snapshot
                .extended_hook_flags
                .get(&existing.handle)
                .copied()
                .unwrap_or(ExtendedDynamicFlags {
                    has_on_add_effective: existing.has_on_add_hook,
                    has_on_remove: existing.has_on_remove_hook,
                    ..ExtendedDynamicFlags::default()
                });
            if existing_flags != new_flags {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "extended hook/clone presence mismatch on re-registration (existing={existing_flags:?}, new={new_flags:?})"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Registers a host [`Component`] implementing [`ModComponent`] with [`ComponentOptions`],
    /// returning its 1-based [`BamComponent`] handle.
    pub fn register_component<T: Component + ModComponent>(
        &mut self,
        comp_id: ComponentId,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.register_component_with_world_flags::<T>(comp_id, options, 0)
    }

    /// Internal helper merging Bevy's `ComponentInfo` storage/mutability flags (`SPARSE_SET`, `IMMUTABLE`)
    /// with `ComponentOptions<T>`.
    pub fn register_component_with_world_flags<T: Component + ModComponent>(
        &mut self,
        comp_id: ComponentId,
        options: ComponentOptions<T>,
        inferred_storage_flags: u32,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = (options.extra_storage_flags
            | inferred_storage_flags
            | component_flags_to_storage_flags(T::COMPONENT_FLAGS))
            & !BAM_STORAGE_FLAG_RESOURCE;
        let insert_raw_fn: Option<InsertRawFn> =
            if options.allow_raw_insert && !core::mem::needs_drop::<T>() {
                Some(insert_raw_impl::<T>)
            } else {
                None
            };
        self.insert_host_entry::<T>(
            comp_id,
            storage_flags,
            insert_raw_fn,
            options.insert_default_fn,
        )
    }

    /// Registers a singleton Bevy [`Resource`] implementing [`ModComponent`], returning its
    /// 1-based [`BamComponent`] handle.
    pub fn register_resource<R: Resource + ModComponent>(
        &mut self,
        comp_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags =
            component_flags_to_storage_flags(R::COMPONENT_FLAGS) | BAM_STORAGE_FLAG_RESOURCE;
        self.insert_host_entry::<R>(comp_id, storage_flags, None, None)
    }

    /// Registers a buffered POD Bevy [`Message`] type (`Messages<M>`) implementing [`ModComponent`]
    /// and [`AbiPod`], returning its 1-based [`BamComponent`] handle.
    pub fn register_message<M: Message + ModComponent + AbiPod>(
        &mut self,
        messages_res_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.register_message_inner::<M>(
            messages_res_id,
            true,
            Some(msg_read_batch_impl::<M>),
            Some(msg_send_impl::<M>),
            Some(msg_write_batch_impl::<M>),
        )
    }

    /// Registers a VTable-backed non-POD Bevy [`Message`] type (`Messages<M>`) implementing
    /// [`ModComponent`] + [`Default`] (e.g. containing `String` or `Vec<T>` fields), returning its
    /// 1-based [`BamComponent`] handle.
    ///
    /// - `msg_read_batch` writes `*const c_void` (`usize`) pointers to host `&M` items inside
    ///   `Messages<M>`, which guests inspect through the exported `"bam.v0.<M>.export_fields"`
    ///   witness symbol.
    /// - `msg_send` accepts field-witness packets `[u32 field_count; (name, tag, payload)*]` and
    ///   reconstructs `M` on the host allocator via `"bam.v0.<M>.import_field"`.
    pub fn register_non_pod_message<M: Message + ModComponent + Default>(
        &mut self,
        messages_res_id: ComponentId,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.register_message_inner::<M>(
            messages_res_id,
            false,
            Some(msg_read_batch_non_pod_impl::<M>),
            Some(msg_send_non_pod_impl::<M>),
            None,
        )
    }

    fn register_message_inner<M: Message + ModComponent>(
        &mut self,
        messages_res_id: ComponentId,
        is_abi_pod: bool,
        msg_read_batch_fn: Option<MsgReadBatchFn>,
        msg_send_fn: Option<MsgSendFn>,
        msg_write_batch_fn: Option<MsgWriteBatchFn>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags =
            component_flags_to_storage_flags(M::COMPONENT_FLAGS) | BAM_STORAGE_FLAG_MESSAGE;
        let name_str = M::CANONICAL_NAME;
        let uuid = M::UUID;
        let (wire_size, wire_align) = if is_abi_pod {
            (M::size(), M::align())
        } else {
            (
                core::mem::size_of::<*const c_void>(),
                core::mem::align_of::<*const c_void>(),
            )
        };
        self.check_registration_collision(
            name_str,
            uuid,
            wire_size,
            wire_align,
            storage_flags,
            Some(messages_res_id),
        )?;

        let sym_addr = SymbolAddress::new(M::erased_vtable_ptr());
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(name_str) {
                let idx = (existing_handle.0 - 1) as usize;
                snap.entries[idx].vtable_ptr = sym_addr;
                snap.entries[idx].size = wire_size;
                snap.entries[idx].align = wire_align;
                snap.entries[idx].schema_fingerprint = M::SCHEMA_FINGERPRINT;
                snap.entries[idx].fields = Arc::from(M::BAM_FIELDS);
                snap.entries[idx].owned_field_strings = None;
                snap.entries[idx].is_abi_pod = is_abi_pod;
                snap.entries[idx].create_msg_cursor_fn = Some(create_msg_cursor_impl::<M>);
                snap.entries[idx].msg_read_batch_fn = msg_read_batch_fn;
                snap.entries[idx].msg_send_fn = msg_send_fn;
                snap.entries[idx].msg_write_batch_fn = msg_write_batch_fn;
                existing_handle
            } else {
                snap.alloc_handle_and_insert(
                    NewComponentBase {
                        name: name_str,
                        uuid,
                        component_id: messages_res_id,
                        size: wire_size,
                        align: wire_align,
                        storage_flags,
                        is_abi_pod,
                    },
                    |e| {
                        e.schema_fingerprint = M::SCHEMA_FINGERPRINT;
                        e.fields = Arc::from(M::BAM_FIELDS);
                        e.vtable_ptr = sym_addr;
                        e.create_msg_cursor_fn = Some(create_msg_cursor_impl::<M>);
                        e.msg_read_batch_fn = msg_read_batch_fn;
                        e.msg_send_fn = msg_send_fn;
                        e.msg_write_batch_fn = msg_write_batch_fn;
                    },
                )?
            };
            Self::insert_mod_component_symbols::<M>(snap);
            if !is_abi_pod {
                let marker_sym = format!("bam.v0.{}.non_pod_message", M::CANONICAL_NAME);
                snap.symbols.insert(
                    Arc::from(marker_sym.as_str()),
                    SymbolRecord {
                        addr: SymbolAddress::new(msg_read_batch_non_pod_impl::<M> as *const c_void),
                        owner: None,
                    },
                );
            }
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
    }

    /// Registers Bevy's built-in [`ChildOf`] relationship component under [`BAM_CHILD_OF_NAME`]
    /// (plus `"ChildOf"` alias if unclaimed) with [`FfiChildOf`] wire layout (`validates_entity_payload = true`).
    pub fn register_hierarchy(
        &mut self,
        child_of_id: ComponentId,
        is_immutable: bool,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = if is_immutable {
            BAM_STORAGE_FLAG_IMMUTABLE
        } else {
            0
        };
        let uuid = FfiChildOf::UUID;
        self.check_registration_collision(
            BAM_CHILD_OF_NAME,
            uuid,
            core::mem::size_of::<FfiChildOf>(),
            core::mem::align_of::<FfiChildOf>(),
            storage_flags,
            Some(child_of_id),
        )?;

        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(BAM_CHILD_OF_NAME) {
                let idx = (existing_handle.0 - 1) as usize;
                snap.entries[idx].insert_raw_fn = Some(insert_child_of_raw);
                snap.entries[idx].validates_entity_payload = true;
                snap.entries[idx].is_dynamic = false;
                snap.entries[idx].owner = None;
                existing_handle
            } else {
                snap.alloc_handle_and_insert(
                    NewComponentBase {
                        name: BAM_CHILD_OF_NAME,
                        uuid,
                        component_id: child_of_id,
                        size: core::mem::size_of::<FfiChildOf>(),
                        align: core::mem::align_of::<FfiChildOf>(),
                        storage_flags,
                        is_abi_pod: true,
                    },
                    |e| {
                        e.insert_raw_fn = Some(insert_child_of_raw);
                        e.validates_entity_payload = true;
                    },
                )?
            };
            if !snap.by_name.contains_key("ChildOf") {
                snap.by_name.insert(Arc::from("ChildOf"), handle);
            }
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
    }

    /// Registers a triggerable host Bevy [`Event`] implementing [`ModComponent`] + [`Copy`],
    /// returning its 1-based [`BamComponent`] handle.
    pub fn register_event<E>(
        &mut self,
        event_key: EventKey,
    ) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy,
    {
        self.register_event_internal::<E>(
            event_key,
            BAM_STORAGE_FLAG_EVENT,
            Some(trigger_event_impl::<E>),
            None,
        )
    }

    /// Registers a triggerable entity-targeted Bevy [`EntityEvent`] implementing [`ModComponent`] + [`Copy`],
    /// returning its 1-based [`BamComponent`] handle.
    pub fn register_entity_event<E>(
        &mut self,
        event_key: EventKey,
    ) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default,
    {
        self.register_event_internal::<E>(
            event_key,
            BAM_STORAGE_FLAG_EVENT | BAM_STORAGE_FLAG_ENTITY_EVENT,
            Some(trigger_entity_event_impl::<E>),
            Some(extract_entity_event_target_impl::<E>),
        )
    }

    fn register_event_internal<E: ModComponent + Copy>(
        &mut self,
        event_key: EventKey,
        extra_flags: u32,
        trigger_event_fn: Option<TriggerEventFn>,
        extract_event_target_fn: Option<ExtractEventTargetFn>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let storage_flags = component_flags_to_storage_flags(E::COMPONENT_FLAGS) | extra_flags;
        let comp_id = event_key.component_id();
        let name_str = E::CANONICAL_NAME;
        let uuid = E::UUID;
        self.check_registration_collision(
            name_str,
            uuid,
            E::size(),
            E::align(),
            storage_flags,
            Some(comp_id),
        )?;

        let sym_addr = SymbolAddress::new(E::erased_vtable_ptr());
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(name_str) {
                let idx = (existing_handle.0 - 1) as usize;
                snap.entries[idx].vtable_ptr = sym_addr;
                snap.entries[idx].schema_fingerprint = E::SCHEMA_FINGERPRINT;
                snap.entries[idx].fields = Arc::from(E::BAM_FIELDS);
                snap.entries[idx].owned_field_strings = None;
                snap.entries[idx].event_key = Some(event_key);
                snap.entries[idx].entity_event_trigger_fn = None;
                snap.entries[idx].trigger_event_fn = trigger_event_fn;
                snap.entries[idx].extract_event_target_fn = extract_event_target_fn;
                existing_handle
            } else {
                snap.alloc_handle_and_insert(
                    NewComponentBase {
                        name: name_str,
                        uuid,
                        component_id: comp_id,
                        size: E::size(),
                        align: E::align(),
                        storage_flags,
                        is_abi_pod: E::IS_ABI_POD,
                    },
                    |e| {
                        e.schema_fingerprint = E::SCHEMA_FINGERPRINT;
                        e.fields = Arc::from(E::BAM_FIELDS);
                        e.vtable_ptr = sym_addr;
                        e.event_key = Some(event_key);
                        e.trigger_event_fn = trigger_event_fn;
                        e.extract_event_target_fn = extract_event_target_fn;
                    },
                )?
            };
            Self::insert_mod_component_symbols::<E>(snap);
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
    }

    fn insert_host_entry<T: Component + ModComponent>(
        &mut self,
        comp_id: ComponentId,
        storage_flags: u32,
        insert_raw_fn: Option<InsertRawFn>,
        insert_default_fn: Option<InsertDefaultFn>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let name_str = T::CANONICAL_NAME;
        let uuid = T::UUID;
        self.check_registration_collision(
            name_str,
            uuid,
            T::size(),
            T::align(),
            storage_flags,
            Some(comp_id),
        )?;

        let sym_addr = SymbolAddress::new(T::erased_vtable_ptr());
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(name_str) {
                let idx = (existing_handle.0 - 1) as usize;
                let merged_default = insert_default_fn.or(snap.entries[idx].insert_default_fn);
                let merged_raw = insert_raw_fn.or(snap.entries[idx].insert_raw_fn);
                snap.entries[idx].vtable_ptr = sym_addr;
                snap.entries[idx].schema_fingerprint = T::SCHEMA_FINGERPRINT;
                snap.entries[idx].fields = Arc::from(T::BAM_FIELDS);
                snap.entries[idx].owned_field_strings = None;
                snap.entries[idx].insert_raw_fn = merged_raw;
                snap.entries[idx].insert_default_fn = merged_default;
                snap.entries[idx].is_dynamic = false;
                snap.entries[idx].owner = None;
                existing_handle
            } else {
                snap.alloc_handle_and_insert(
                    NewComponentBase {
                        name: name_str,
                        uuid,
                        component_id: comp_id,
                        size: T::size(),
                        align: T::align(),
                        storage_flags,
                        is_abi_pod: T::IS_ABI_POD,
                    },
                    |e| {
                        e.schema_fingerprint = T::SCHEMA_FINGERPRINT;
                        e.fields = Arc::from(T::BAM_FIELDS);
                        e.vtable_ptr = sym_addr;
                        e.insert_raw_fn = insert_raw_fn;
                        e.insert_default_fn = insert_default_fn;
                    },
                )?
            };

            Self::insert_mod_component_symbols::<T>(snap);
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
    }

    fn insert_mod_component_symbols<T: ModComponent>(snap: &mut RegistrySnapshot) {
        let vtable_sym = format!("bam.v0.{}.vtable", T::CANONICAL_NAME);
        let vtable_addr = SymbolAddress::new(T::erased_vtable_ptr());
        if !vtable_addr.is_null() {
            snap.symbols.insert(
                Arc::from(vtable_sym.as_str()),
                SymbolRecord {
                    addr: vtable_addr,
                    owner: None,
                },
            );
        }
        for sym in T::symbols() {
            if sym.name.starts_with("bam.v0.") && !sym.ptr.is_null() {
                snap.symbols.insert(
                    Arc::from(sym.name),
                    SymbolRecord {
                        addr: SymbolAddress::new(sym.ptr),
                        owner: None,
                    },
                );
            }
        }
    }

    /// Registers a non-component [`ModComponent`] helper/nested struct type (e.g. `Vec3f`)
    /// so its static `"bam.v0.<name>.vtable"` and accessor symbols are resolvable via `get_proc_address`.
    pub fn register_type<T: ModComponent>(&mut self) {
        let snap = Arc::make_mut(&mut self.snapshot);
        Self::insert_mod_component_symbols::<T>(snap);
        self.publish_after_mutation();
    }

    /// Registers a guest-owned dynamic component described by `reg`, returning its 1-based
    /// [`BamComponent`] handle.
    pub(crate) fn register_dynamic_component(
        &mut self,
        reg: DynamicComponentReg<'_>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let DynamicComponentReg {
            comp_id,
            desc,
            name,
            owner,
            drop_slot,
            required_components,
            schema_fingerprint,
            fields,
            owned_field_strings,
        } = reg;
        let storage_flags = component_flags_to_storage_flags(desc.flags);
        let expected_comp_id = self
            .snapshot
            .get_by_name(name)
            .filter(|entry| entry.is_dynamic)
            .map_or(comp_id, |entry| entry.component_id);
        self.check_registration_collision(
            name,
            desc.uuid,
            desc.size,
            desc.align,
            storage_flags,
            Some(expected_comp_id),
        )?;
        self.check_dynamic_drop_compatibility(name, desc.drop_fn.is_some())?;
        self.check_dynamic_extended_hook_compatibility(name, desc)?;

        let ext_flags = ExtendedDynamicFlags {
            has_on_add_effective: desc.on_add.is_some() || !required_components.is_empty(),
            has_on_insert: desc.on_insert.is_some(),
            has_on_discard: desc.on_discard.is_some(),
            has_on_remove: desc.on_remove.is_some(),
            has_on_despawn: desc.on_despawn.is_some(),
            has_clone_fn: desc.clone_fn.is_some(),
        };

        let sym_addr = SymbolAddress::new(desc.vtable_ptr);
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let mut is_dynamic_entry = true;
            let mut replaced_comp_id: Option<(ComponentId, ComponentId)> = None;
            let handle = if let Some(&existing_handle) = snap.by_name.get(name) {
                let idx = (existing_handle.0 - 1) as usize;
                let entry = &mut snap.entries[idx];
                is_dynamic_entry = entry.is_dynamic;
                if entry.is_dynamic {
                    if entry.component_id != comp_id {
                        replaced_comp_id = Some((entry.component_id, comp_id));
                    }
                    entry.component_id = comp_id;
                    entry.schema_fingerprint = schema_fingerprint;
                    entry.fields = Arc::clone(&fields);
                    entry.owned_field_strings = owned_field_strings;
                    entry.vtable_ptr = sym_addr;
                    entry.drop_fn = desc.drop_fn;
                    entry.drop_slot = drop_slot;
                    // SAFETY: `comp_id` was registered in `world` with `(desc.size, desc.align)`
                    // and is safe for dynamic event dispatch.
                    entry.event_key = Some(unsafe { EventKey::new(comp_id) });
                    entry.owner = owner;
                }
                existing_handle
            } else {
                snap.alloc_handle_and_insert(
                    NewComponentBase {
                        name,
                        uuid: desc.uuid,
                        component_id: comp_id,
                        size: desc.size,
                        align: desc.align,
                        storage_flags,
                        is_abi_pod: desc.is_pod(),
                    },
                    |e| {
                        e.schema_fingerprint = schema_fingerprint;
                        e.fields = fields;
                        e.owned_field_strings = owned_field_strings;
                        e.is_dynamic = true;
                        e.vtable_ptr = sym_addr;
                        e.drop_fn = desc.drop_fn;
                        e.drop_slot = drop_slot;
                        e.has_on_add_hook = ext_flags.has_on_add_effective;
                        e.has_on_remove_hook = ext_flags.has_on_remove;
                        // SAFETY: `comp_id` was registered via `world.register_component_with_descriptor`
                        // with layout `(desc.size, desc.align)` and is safe for dynamic event dispatch.
                        e.event_key = Some(unsafe { EventKey::new(comp_id) });
                        e.owner = owner;
                    },
                )?
            };

            if let Some((old_comp_id, new_comp_id)) = replaced_comp_id {
                snap.by_component_id.remove(&old_comp_id);
                snap.by_component_id.insert(new_comp_id, handle);
            }

            if is_dynamic_entry {
                snap.extended_hook_flags.insert(handle, ext_flags);
                if required_components.is_empty() {
                    snap.required_components.remove(&handle);
                } else {
                    snap.required_components.insert(handle, required_components);
                }
                let vtable_key: Arc<str> = Arc::from(format!("bam.v0.{name}.vtable").as_str());
                if !sym_addr.is_null() {
                    snap.symbols.insert(
                        vtable_key,
                        SymbolRecord {
                            addr: sym_addr,
                            owner,
                        },
                    );
                } else if snap
                    .symbols
                    .get(vtable_key.as_ref())
                    .is_some_and(|r| r.owner.is_some())
                {
                    snap.symbols.remove(vtable_key.as_ref());
                }
            }
            handle
        };
        self.publish_after_mutation();

        Ok(handle)
    }

    /// Registers a guest-owned dynamic singleton resource from a [`ComponentDesc`], returning its
    /// 1-based [`BamComponent`] handle (`BAM_STORAGE_FLAG_RESOURCE`).
    pub(crate) fn register_resource_from_desc(
        &mut self,
        reg: DynamicResourceReg<'_>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let DynamicResourceReg {
            comp_id,
            desc,
            name,
            owner,
            drop_slot,
            schema_fingerprint,
            fields,
            owned_field_strings,
        } = reg;
        let storage_flags =
            component_flags_to_storage_flags(desc.flags) | BAM_STORAGE_FLAG_RESOURCE;
        let expected_comp_id = self
            .snapshot
            .get_by_name(name)
            .filter(|entry| entry.is_dynamic)
            .map_or(comp_id, |entry| entry.component_id);
        self.check_registration_collision(
            name,
            desc.uuid,
            desc.size,
            desc.align,
            storage_flags,
            Some(expected_comp_id),
        )?;
        self.check_dynamic_drop_compatibility(name, desc.drop_fn.is_some())?;

        let sym_addr = SymbolAddress::new(desc.vtable_ptr);
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(name) {
                let idx = (existing_handle.0 - 1) as usize;
                let entry = &mut snap.entries[idx];
                if entry.is_dynamic {
                    if entry.component_id != comp_id {
                        let old_id = entry.component_id;
                        entry.component_id = comp_id;
                        snap.by_component_id.remove(&old_id);
                        snap.by_component_id.insert(comp_id, existing_handle);
                    }
                    entry.schema_fingerprint = schema_fingerprint;
                    entry.fields = Arc::clone(&fields);
                    entry.owned_field_strings = owned_field_strings;
                    entry.vtable_ptr = sym_addr;
                    entry.drop_fn = desc.drop_fn;
                    entry.drop_slot = drop_slot;
                    entry.owner = owner;
                }
                existing_handle
            } else {
                snap.alloc_handle_and_insert(
                    NewComponentBase {
                        name,
                        uuid: desc.uuid,
                        component_id: comp_id,
                        size: desc.size,
                        align: desc.align,
                        storage_flags,
                        is_abi_pod: desc.is_pod(),
                    },
                    |e| {
                        e.schema_fingerprint = schema_fingerprint;
                        e.fields = fields;
                        e.owned_field_strings = owned_field_strings;
                        e.is_dynamic = true;
                        e.vtable_ptr = sym_addr;
                        e.drop_fn = desc.drop_fn;
                        e.drop_slot = drop_slot;
                        e.owner = owner;
                    },
                )?
            };
            let vtable_key: Arc<str> = Arc::from(format!("bam.v0.{name}.vtable").as_str());
            if !sym_addr.is_null() {
                snap.symbols.insert(
                    vtable_key,
                    SymbolRecord {
                        addr: sym_addr,
                        owner,
                    },
                );
            }
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
    }

    /// Registers a guest-owned dynamic buffered message stream from a [`ComponentDesc`], creating
    /// a dedicated [`DynamicMessageBuffer`] resource in `world` and returning its [`BamComponent`].
    pub(crate) fn register_message_from_desc(
        &mut self,
        world: &mut World,
        reg: DynamicMessageReg<'_>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        let DynamicMessageReg {
            desc,
            name,
            owner,
            is_active,
            schema_fingerprint,
            fields,
            owned_field_strings,
        } = reg;
        let storage_flags = component_flags_to_storage_flags(desc.flags) | BAM_STORAGE_FLAG_MESSAGE;
        let existing_id = self
            .snapshot
            .get_by_name(name)
            .map(|entry| entry.component_id);
        self.check_registration_collision(
            name,
            desc.uuid,
            desc.size,
            desc.align,
            storage_flags,
            existing_id,
        )?;
        self.check_dynamic_drop_compatibility(name, desc.drop_fn.is_some())?;

        let msg_res_id = if let Some(comp_id) = existing_id {
            if let Some(res_ent) = world.resource_entities().get(comp_id)
                && let Ok(mut ent_mut) = world.get_entity_mut(res_ent)
                && let Ok(untyped) = ent_mut.get_mut_by_id(comp_id)
            {
                // SAFETY: `comp_id` was registered in `world` using `DynamicMessageBuffer`'s layout.
                let mut buf = unsafe { untyped.with_type::<DynamicMessageBuffer>() };
                buf.refresh_drop_binding(desc.drop_fn, Arc::clone(&is_active));
            } else {
                if world.resource_entities().get(comp_id).is_none() {
                    world.spawn(bevy_ecs::resource::IsResource::new(comp_id));
                }
                let mut buf = core::mem::ManuallyDrop::new(DynamicMessageBuffer::new(
                    desc.size,
                    desc.align,
                    desc.drop_fn,
                    Arc::clone(&is_active),
                ));
                let nn = NonNull::from(&mut *buf).cast::<u8>();
                // SAFETY: `nn` points to an initialized, ownership-transferred `DynamicMessageBuffer`.
                let owning = unsafe { OwningPtr::new(nn) };
                // SAFETY: `comp_id` was registered in `world` with `Layout::new::<DynamicMessageBuffer>()`.
                unsafe {
                    world.insert_resource_by_id(comp_id, owning, MaybeLocation::caller());
                }
            }
            comp_id
        } else {
            // SAFETY: `Layout::new::<DynamicMessageBuffer>()` is exact for `DynamicMessageBuffer`, and
            // `drop_dynamic_message_buffer` consumes `OwningPtr` via `ptr.read::<DynamicMessageBuffer>()`.
            let bevy_desc = unsafe {
                ComponentDescriptor::new_with_layout(
                    format!("bam::dynamic_msg::{name}"),
                    StorageType::Table,
                    Layout::new::<DynamicMessageBuffer>(),
                    Some(drop_dynamic_message_buffer),
                    true,
                    false,
                    ComponentCloneBehavior::Ignore,
                    None,
                )
            };
            let new_id = world.register_component_with_descriptor(bevy_desc);
            if world.resource_entities().get(new_id).is_none() {
                world.spawn(bevy_ecs::resource::IsResource::new(new_id));
            }
            let mut buf = core::mem::ManuallyDrop::new(DynamicMessageBuffer::new(
                desc.size,
                desc.align,
                desc.drop_fn,
                is_active,
            ));
            let nn = NonNull::from(&mut *buf).cast::<u8>();
            // SAFETY: `nn` points to an initialized, ownership-transferred `DynamicMessageBuffer`.
            let owning = unsafe { OwningPtr::new(nn) };
            // SAFETY: `new_id` was just registered with `Layout::new::<DynamicMessageBuffer>()`.
            unsafe {
                world.insert_resource_by_id(new_id, owning, MaybeLocation::caller());
            }
            new_id
        };

        let sym_addr = SymbolAddress::new(desc.vtable_ptr);
        let handle = {
            let snap = Arc::make_mut(&mut self.snapshot);
            let handle = if let Some(&existing_handle) = snap.by_name.get(name) {
                let idx = (existing_handle.0 - 1) as usize;
                let entry = &mut snap.entries[idx];
                if entry.is_dynamic {
                    entry.schema_fingerprint = schema_fingerprint;
                    entry.fields = Arc::clone(&fields);
                    entry.owned_field_strings = owned_field_strings;
                    entry.vtable_ptr = sym_addr;
                    entry.drop_fn = desc.drop_fn;
                    entry.create_msg_cursor_fn = Some(dynamic_create_msg_cursor);
                    entry.msg_read_batch_fn = Some(dynamic_msg_read_batch);
                    entry.msg_send_fn = Some(dynamic_msg_send);
                    entry.msg_write_batch_fn = Some(dynamic_msg_write_batch);
                    entry.owner = owner;
                }
                existing_handle
            } else {
                snap.alloc_handle_and_insert(
                    NewComponentBase {
                        name,
                        uuid: desc.uuid,
                        component_id: msg_res_id,
                        size: desc.size,
                        align: desc.align,
                        storage_flags,
                        is_abi_pod: desc.is_pod(),
                    },
                    |e| {
                        e.schema_fingerprint = schema_fingerprint;
                        e.fields = fields;
                        e.owned_field_strings = owned_field_strings;
                        e.is_dynamic = true;
                        e.vtable_ptr = sym_addr;
                        e.drop_fn = desc.drop_fn;
                        e.create_msg_cursor_fn = Some(dynamic_create_msg_cursor);
                        e.msg_read_batch_fn = Some(dynamic_msg_read_batch);
                        e.msg_send_fn = Some(dynamic_msg_send);
                        e.msg_write_batch_fn = Some(dynamic_msg_write_batch);
                        e.owner = owner;
                    },
                )?
            };
            let vtable_key: Arc<str> = Arc::from(format!("bam.v0.{name}.vtable").as_str());
            if !sym_addr.is_null() {
                snap.symbols.insert(
                    vtable_key,
                    SymbolRecord {
                        addr: sym_addr,
                        owner,
                    },
                );
            }
            handle
        };
        self.publish_after_mutation();
        Ok(handle)
    }

    /// Registers a custom `"bam.v0.<name>"` host symbol scoped strictly to this [`ModRegistry`].
    ///
    /// If `name` does not already begin with `"bam.v0."`, it is automatically prefixed with `"bam.v0."`.
    ///
    /// # Safety
    /// Caller must ensure `ptr` remains valid for any guest code resolving `name` against this registry.
    pub unsafe fn register_custom_symbol(&mut self, name: &str, ptr: *const c_void) {
        let full_name: Arc<str> = if name.starts_with("bam.v0.") {
            Arc::from(name)
        } else {
            Arc::from(format!("bam.v0.{name}").as_str())
        };
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.symbols.insert(
            full_name,
            SymbolRecord {
                addr: SymbolAddress::new(ptr),
                owner: None,
            },
        );
        self.publish_after_mutation();
    }

    /// Registers a custom `"bam.v0.<name>"` symbol owned by `owner` (`ModId`) so it is automatically
    /// evicted when `owner` unloads.
    ///
    /// # Safety
    /// Caller must ensure `ptr` remains valid until `owner` is evicted from this registry.
    pub unsafe fn register_mod_symbol(&mut self, name: &str, ptr: *const c_void, owner: ModId) {
        let full_name: Arc<str> = if name.starts_with("bam.v0.") {
            Arc::from(name)
        } else {
            Arc::from(format!("bam.v0.{name}").as_str())
        };
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.symbols.insert(
            full_name,
            SymbolRecord {
                addr: SymbolAddress::new(ptr),
                owner: Some(owner),
            },
        );
        self.publish_after_mutation();
    }

    /// Evicts all guest-owned VTable pointers, drop callbacks, and custom symbols owned by `mod_id`
    /// while preserving 1-based [`BamComponent`] handles and Bevy [`ComponentId`]s across reloads.
    pub(crate) fn evict_mod(&mut self, mod_id: ModId) {
        let snap = Arc::make_mut(&mut self.snapshot);
        snap.symbols.retain(|_, rec| rec.owner != Some(mod_id));
        for comp in &mut snap.entries {
            if comp.owner == Some(mod_id) {
                comp.vtable_ptr = SymbolAddress::NULL;
                comp.drop_fn = None;
                comp.owner = None;
            }
        }
        self.publish_after_mutation();
    }

    /// Looks up registered component metadata in `O(1)` by 1-based [`BamComponent`] handle.
    #[inline]
    pub fn get_by_handle(&self, handle: BamComponent) -> Option<&RegisteredComponent> {
        self.snapshot.get_by_handle(handle)
    }

    /// Looks up registered component metadata by runtime Bevy [`ComponentId`].
    #[inline]
    pub fn get_by_id(&self, id: ComponentId) -> Option<&RegisteredComponent> {
        self.snapshot.get_by_id(id)
    }

    /// Looks up registered component metadata by canonical string name.
    #[inline]
    pub fn get_by_name(&self, name: &str) -> Option<&RegisteredComponent> {
        self.snapshot.get_by_name(name)
    }

    /// Looks up registered component metadata by deterministic 128-bit [`ComponentUuid`].
    #[inline]
    pub fn get_by_uuid(&self, uuid: &ComponentUuid) -> Option<&RegisteredComponent> {
        self.snapshot.get_by_uuid(uuid)
    }

    /// Resolves a `"bam.v0.*"` symbol address from this registry.
    #[inline]
    pub fn resolve_symbol(&self, name: &str) -> Option<*const c_void> {
        self.snapshot.resolve_symbol(name)
    }

    /// Validates that registering relationship `(name, uuid, flags, data_size, data_align, max_degree)`
    /// does not collide with an incompatible existing component or relationship entry.
    ///
    /// Returns `Ok(Some(existing))` if an identical relationship is already registered (idempotent
    /// hot-reload re-registration), or `Ok(None)` if this is a brand-new relationship.
    pub(crate) fn check_relation_registration_collision(
        &self,
        name: &str,
        uuid: ComponentUuid,
        flags: u32,
        data_size: usize,
        data_align: usize,
        max_degree: u32,
    ) -> Result<Option<RegisteredRelation>, ComponentCollisionError> {
        if let Some(existing_by_uuid) = self.snapshot.get_by_uuid(&uuid)
            && existing_by_uuid.name.as_ref() != name
        {
            return Err(ComponentCollisionError::UuidCollision {
                existing_name: existing_by_uuid.name.to_string(),
                new_name: name.to_string(),
                uuid,
            });
        }

        if let Some(existing) = self.snapshot.get_by_name(name) {
            if existing.uuid != uuid {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!("UUID mismatch (existing={:?}, new={uuid:?})", existing.uuid),
                });
            }
            let Some(existing_rel) = self.snapshot.get_relation(existing.handle) else {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: "name is already registered as a non-relationship component"
                        .to_string(),
                });
            };
            if existing_rel.flags != flags {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "relation flags mismatch (existing=0x{:x}, new=0x{:x})",
                        existing_rel.flags, flags
                    ),
                });
            }
            if existing_rel.data_size != data_size || existing_rel.data_align != data_align {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "relation payload layout mismatch (existing=({}, {}), new=({}, {}))",
                        existing_rel.data_size, existing_rel.data_align, data_size, data_align
                    ),
                });
            }
            if existing_rel.max_degree != max_degree {
                return Err(ComponentCollisionError::MetadataMismatch {
                    name: name.to_string(),
                    reason: format!(
                        "relation max_degree mismatch (existing={}, new={max_degree})",
                        existing_rel.max_degree
                    ),
                });
            }
            return Ok(Some(existing_rel.clone()));
        }

        Ok(None)
    }

    /// Registers (or idempotently re-registers) a typed entity relationship entry in this registry,
    /// returning its 1-based [`BamComponent`] handle and stored [`RegisteredRelation`] metadata.
    pub(crate) fn register_relation_entry(
        &mut self,
        spec: RelationRegSpec<'_>,
    ) -> Result<RegisteredRelation, ComponentCollisionError> {
        let RelationRegSpec {
            comp_id,
            name,
            uuid,
            flags,
            data_size,
            data_align,
            max_degree,
            owner,
        } = spec;
        let existing_rel = self.check_relation_registration_collision(
            name, uuid, flags, data_size, data_align, max_degree,
        )?;
        let storage_flags = BAM_STORAGE_FLAG_SPARSE_SET | BAM_STORAGE_FLAG_IMMUTABLE;

        let rel_meta = {
            let snap = Arc::make_mut(&mut self.snapshot);
            if let Some(prev) = existing_rel {
                let idx = (prev.handle.0 - 1) as usize;
                snap.entries[idx].owner = owner;
                prev
            } else {
                // Marker component in Bevy ECS is zero-sized `(0, 1)`; edge payload layout is
                // tracked on `RegisteredRelation::{data_size, data_align}`.
                let handle = snap.alloc_handle_and_insert(
                    NewComponentBase {
                        name,
                        uuid,
                        component_id: comp_id,
                        size: 0,
                        align: 1,
                        storage_flags,
                        is_abi_pod: false,
                    },
                    |e| {
                        e.has_on_remove_hook = true;
                        e.owner = owner;
                    },
                )?;
                let rel = RegisteredRelation {
                    handle,
                    name: Arc::clone(&snap.entries[(handle.0 - 1) as usize].name),
                    uuid,
                    component_id: comp_id,
                    flags,
                    data_size,
                    data_align,
                    max_degree,
                };
                snap.relations.insert(handle, rel.clone());
                rel
            }
        };
        self.publish_after_mutation();
        Ok(rel_meta)
    }

    /// Returns the required [`BamComponent`] handles registered for `handle` (empty if none).
    #[inline]
    pub fn required_components(&self, handle: BamComponent) -> &[BamComponent] {
        self.snapshot.required_components(handle)
    }

    /// Looks up registered relationship metadata in `O(1)` by its 1-based [`BamComponent`] handle.
    #[inline]
    pub fn get_relation(&self, handle: BamComponent) -> Option<&RegisteredRelation> {
        self.snapshot.get_relation(handle)
    }

    /// Looks up registered relationship metadata by its runtime Bevy marker [`ComponentId`].
    #[inline]
    pub fn get_relation_by_id(&self, id: ComponentId) -> Option<&RegisteredRelation> {
        self.snapshot.get_relation_by_id(id)
    }

    /// Looks up registered relationship metadata by deterministic 128-bit [`ComponentUuid`].
    #[inline]
    pub fn get_relation_by_uuid(&self, uuid: &ComponentUuid) -> Option<&RegisteredRelation> {
        self.snapshot.get_relation_by_uuid(uuid)
    }

    /// Returns `true` if `handle` refers to a relationship registered via `host_register_relation`.
    #[inline]
    pub fn is_relation(&self, handle: BamComponent) -> bool {
        self.snapshot.is_relation(handle)
    }

    /// Returns the total number of registered relationships in this registry.
    #[inline]
    pub fn relation_count(&self) -> usize {
        self.snapshot.relation_count()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/catalog_coverage_tests.rs"]
mod coverage_tests;

#[cfg(test)]
mod cursor_tests {
    use super::{
        FfiResult, Message, World, c_void, clamp_batch_max_count, create_msg_cursor_impl,
        drain_cursor_bounded, msg_read_batch_impl, msg_read_batch_non_pod_impl,
    };
    use bamabi_macros::AbiPod;
    use bevy_ecs::message::Messages;

    struct NextAdvancesCursor {
        pos: usize,
        total: usize,
    }

    impl Iterator for NextAdvancesCursor {
        type Item = usize;

        fn next(&mut self) -> Option<Self::Item> {
            if self.pos >= self.total {
                return None;
            }
            let item = self.pos;
            self.pos += 1;
            Some(item)
        }
    }

    #[repr(C)]
    #[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq, AbiPod)]
    struct SeqPod {
        seq: u64,
        tag: u32,
        pad: u32,
    }

    #[repr(C)]
    #[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq, AbiPod)]
    struct ZstPod;

    #[repr(C, align(16))]
    #[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq, AbiPod)]
    struct Simd16Pod {
        lanes: [u32; 4],
    }

    #[derive(Message, Clone, Debug, Default, PartialEq, Eq)]
    struct NonPodItem {
        label: String,
        tags: Vec<u32>,
        seq: u64,
    }

    #[test]
    fn drain_cursor_bounded_conserves_all_items_across_boundary_batches() {
        for total in [0usize, 1, 2, 7, 63, 64, 65, 127, 128, 129, 250] {
            let mut zero_cursor = NextAdvancesCursor { pos: 0, total };
            let mut visited_zero = false;
            let zero_n = drain_cursor_bounded(&mut zero_cursor, 0, |_, _| {
                visited_zero = true;
            });
            assert_eq!(zero_n, 0);
            assert!(!visited_zero);
            assert_eq!(zero_cursor.pos, 0);

            for batch_size in [1usize, 2, 3, 7, 63, 64, 65, 128, usize::MAX] {
                let mut cursor = NextAdvancesCursor { pos: 0, total };
                let mut collected = Vec::with_capacity(total);
                loop {
                    let expected_start = collected.len();
                    let n = drain_cursor_bounded(&mut cursor, batch_size, |idx, item| {
                        assert_eq!(item, expected_start + idx);
                        collected.push(item);
                    });
                    assert!(n <= batch_size);
                    if n == 0 {
                        break;
                    }
                }
                assert_eq!(cursor.pos, total);
                assert_eq!(collected.len(), total);
                for (idx, &val) in collected.iter().enumerate() {
                    assert_eq!(val, idx);
                }
            }
        }
    }

    #[test]
    fn clamp_batch_max_count_zst_and_overflow_bounds() {
        assert_eq!(clamp_batch_max_count(0, 0), 0);
        assert_eq!(clamp_batch_max_count(7, 0), 7);
        assert_eq!(clamp_batch_max_count(usize::MAX, 0), usize::MAX);
        assert_eq!(clamp_batch_max_count(100, 16), 100);
        assert_eq!(
            clamp_batch_max_count(usize::MAX, 16),
            (isize::MAX as usize) / 16
        );

        for elem_size in [
            1usize,
            4,
            8,
            16,
            64,
            isize::MAX as usize,
            (isize::MAX as usize) + 1,
        ] {
            for max_count in [0usize, 1, 64, isize::MAX as usize, usize::MAX] {
                let eff = clamp_batch_max_count(max_count, elem_size);
                assert!(eff <= max_count);
                assert!(
                    eff.checked_mul(elem_size)
                        .is_some_and(|bytes| bytes <= isize::MAX as usize)
                );
            }
        }
    }

    #[test]
    fn msg_read_batch_impl_pod_zst_unaligned_simd_and_usize_max() {
        // (a) POD struct drained across partial batches
        {
            let mut world = World::new();
            let msg_res_id = world.register_component::<Messages<SeqPod>>();
            world.init_resource::<Messages<SeqPod>>();
            let total = 130usize;
            let sent: Vec<SeqPod> = (0..total)
                .map(|i| SeqPod {
                    seq: i as u64,
                    tag: (i as u32).wrapping_mul(17),
                    pad: 0xA5A5_5A5A,
                })
                .collect();
            {
                let mut msgs = world.resource_mut::<Messages<SeqPod>>();
                for &m in &sent {
                    msgs.write(m);
                }
            }

            let cell = world.as_unsafe_world_cell_readonly();
            for batch_size in [1usize, 7, 64, 65] {
                let mut cursor = create_msg_cursor_impl::<SeqPod>();
                let mut collected = Vec::with_capacity(total);
                let mut out_buf = vec![SeqPod::default(); batch_size];
                loop {
                    let mut out_count = usize::MAX;
                    // SAFETY: `cell` grants shared read access to `Messages<SeqPod>`, `cursor` was
                    // created by `create_msg_cursor_impl::<SeqPod>`, and `out_buf` has `batch_size`
                    // writable `SeqPod` elements.
                    let rc = unsafe {
                        msg_read_batch_impl::<SeqPod>(
                            cell,
                            msg_res_id,
                            cursor.as_mut(),
                            out_buf.as_mut_ptr().cast::<u8>(),
                            batch_size,
                            &raw mut out_count,
                        )
                    };
                    assert_eq!(rc, FfiResult::OK);
                    assert!(out_count <= batch_size);
                    if out_count == 0 {
                        break;
                    }
                    collected.extend_from_slice(&out_buf[..out_count]);
                }
                assert_eq!(collected, sent);
            }

            // (d) `max_count = usize::MAX` on a small valid buffer sized to queued messages.
            {
                let mut cursor = create_msg_cursor_impl::<SeqPod>();
                let mut out_buf = vec![SeqPod::default(); total];
                let mut out_count = 0usize;
                // SAFETY: `out_buf` has `total` writable elements and `Messages<SeqPod>` holds `total`
                // messages, so `msg_read_batch_impl` writes at most `total` slots without reading or
                // writing beyond `out_buf`.
                let rc = unsafe {
                    msg_read_batch_impl::<SeqPod>(
                        cell,
                        msg_res_id,
                        cursor.as_mut(),
                        out_buf.as_mut_ptr().cast::<u8>(),
                        usize::MAX,
                        &raw mut out_count,
                    )
                };
                assert_eq!(rc, FfiResult::OK);
                assert_eq!(out_count, total);
                assert_eq!(out_buf, sent);

                let mut trailing = usize::MAX;
                // SAFETY: Trailing read after drain yields 0 items without writing to `out_buf`.
                let rc_end = unsafe {
                    msg_read_batch_impl::<SeqPod>(
                        cell,
                        msg_res_id,
                        cursor.as_mut(),
                        out_buf.as_mut_ptr().cast::<u8>(),
                        usize::MAX,
                        &raw mut trailing,
                    )
                };
                assert_eq!(rc_end, FfiResult::OK);
                assert_eq!(trailing, 0);
            }
        }

        // (b) ZST `Messages<ZstPod>` with null `out_buf`
        {
            let mut world = World::new();
            let msg_res_id = world.register_component::<Messages<ZstPod>>();
            world.init_resource::<Messages<ZstPod>>();
            {
                let mut msgs = world.resource_mut::<Messages<ZstPod>>();
                for _ in 0..5 {
                    msgs.write(ZstPod);
                }
            }
            let cell = world.as_unsafe_world_cell_readonly();
            let mut cursor = create_msg_cursor_impl::<ZstPod>();
            let mut out_count = 0usize;
            // SAFETY: `ZstPod` is zero-sized so `msg_read_batch_impl` permits a null `out_buf`.
            let rc1 = unsafe {
                msg_read_batch_impl::<ZstPod>(
                    cell,
                    msg_res_id,
                    cursor.as_mut(),
                    core::ptr::null_mut(),
                    3,
                    &raw mut out_count,
                )
            };
            assert_eq!(rc1, FfiResult::OK);
            assert_eq!(out_count, 3);

            // SAFETY: Remaining 2 ZST messages drained with null `out_buf` and `max_count = usize::MAX`.
            let rc2 = unsafe {
                msg_read_batch_impl::<ZstPod>(
                    cell,
                    msg_res_id,
                    cursor.as_mut(),
                    core::ptr::null_mut(),
                    usize::MAX,
                    &raw mut out_count,
                )
            };
            assert_eq!(rc2, FfiResult::OK);
            assert_eq!(out_count, 2);

            // SAFETY: Drained cursor returns 0.
            let rc3 = unsafe {
                msg_read_batch_impl::<ZstPod>(
                    cell,
                    msg_res_id,
                    cursor.as_mut(),
                    core::ptr::null_mut(),
                    3,
                    &raw mut out_count,
                )
            };
            assert_eq!(rc3, FfiResult::OK);
            assert_eq!(out_count, 0);
        }

        // (c) `align(16)` SIMD message written into an unaligned destination buffer
        {
            let mut world = World::new();
            let msg_res_id = world.register_component::<Messages<Simd16Pod>>();
            world.init_resource::<Messages<Simd16Pod>>();
            let total = 17usize;
            let sent: Vec<Simd16Pod> = (0..total)
                .map(|i| {
                    let b = i as u32;
                    Simd16Pod {
                        lanes: [b, b + 100, b + 200, b + 300],
                    }
                })
                .collect();
            {
                let mut msgs = world.resource_mut::<Messages<Simd16Pod>>();
                for &m in &sent {
                    msgs.write(m);
                }
            }

            let cell = world.as_unsafe_world_cell_readonly();
            let mut cursor = create_msg_cursor_impl::<Simd16Pod>();
            let batch_size = 5usize;
            let elem_size = core::mem::size_of::<Simd16Pod>();
            let mut raw = vec![0u8; batch_size * elem_size + 16];
            let offset = 3usize;
            let mut collected = Vec::with_capacity(total);
            loop {
                let mut out_count = 0usize;
                // SAFETY: `raw[offset..]` provides `batch_size * size_of::<Simd16Pod>()` writable
                // bytes at an unaligned `+3` offset; `msg_read_batch_impl` uses `write_unaligned`.
                let unaligned_ptr = unsafe { raw.as_mut_ptr().add(offset) };
                // SAFETY: `cell` provides shared read access to `Messages<Simd16Pod>` and `unaligned_ptr`
                // is writable for `batch_size * elem_size` bytes.
                let rc = unsafe {
                    msg_read_batch_impl::<Simd16Pod>(
                        cell,
                        msg_res_id,
                        cursor.as_mut(),
                        unaligned_ptr,
                        batch_size,
                        &raw mut out_count,
                    )
                };
                assert_eq!(rc, FfiResult::OK);
                if out_count == 0 {
                    break;
                }
                for i in 0..out_count {
                    // SAFETY: `msg_read_batch_impl` initialized slot `i` at `unaligned_ptr + i * elem_size`.
                    let item = unsafe {
                        core::ptr::read_unaligned(
                            unaligned_ptr.add(i * elem_size).cast::<Simd16Pod>(),
                        )
                    };
                    collected.push(item);
                }
            }
            assert_eq!(collected, sent);
        }
    }

    #[test]
    fn msg_read_batch_non_pod_impl_preserves_provenance_across_batches() {
        let mut world = World::new();
        let msg_res_id = world.register_component::<Messages<NonPodItem>>();
        world.init_resource::<Messages<NonPodItem>>();
        let total = 19usize;
        let sent: Vec<NonPodItem> = (0..total)
            .map(|i| NonPodItem {
                label: format!("msg_{i}"),
                tags: vec![i as u32, (i as u32) * 3],
                seq: i as u64,
            })
            .collect();
        {
            let mut msgs = world.resource_mut::<Messages<NonPodItem>>();
            for item in &sent {
                msgs.write(item.clone());
            }
        }

        let cell = world.as_unsafe_world_cell_readonly();

        // Pass 1: aligned pointer buffer in partial batches of 3.
        {
            let mut cursor = create_msg_cursor_impl::<NonPodItem>();
            let mut ptrs = [core::ptr::null::<c_void>(); 3];
            let mut collected = Vec::with_capacity(total);
            loop {
                let mut out_count = 0usize;
                // SAFETY: `ptrs` holds 3 writable `*const c_void` slots and `cell` keeps `Messages<NonPodItem>`
                // alive without mutation while returned pointers are read.
                let rc = unsafe {
                    msg_read_batch_non_pod_impl::<NonPodItem>(
                        cell,
                        msg_res_id,
                        cursor.as_mut(),
                        ptrs.as_mut_ptr().cast::<u8>(),
                        ptrs.len(),
                        &raw mut out_count,
                    )
                };
                assert_eq!(rc, FfiResult::OK);
                if out_count == 0 {
                    break;
                }
                for &p in &ptrs[..out_count] {
                    // SAFETY: `p` preserves provenance to a live `NonPodItem` inside `Messages<NonPodItem>`.
                    let item_ref = unsafe { &*p.cast::<NonPodItem>() };
                    collected.push(item_ref.clone());
                }
            }
            assert_eq!(collected, sent);
        }

        // Pass 2: unaligned byte buffer (+3 offset) in partial batches of 7 (Miri Strict Provenance).
        {
            let mut cursor = create_msg_cursor_impl::<NonPodItem>();
            let batch_size = 7usize;
            let ptr_size = core::mem::size_of::<*const c_void>();
            let mut raw = vec![0u8; batch_size * ptr_size + 16];
            let offset = 3usize;
            let mut collected = Vec::with_capacity(total);
            loop {
                let mut out_count = 0usize;
                // SAFETY: `raw[offset..]` has `batch_size * ptr_size` writable bytes at an unaligned offset.
                let unaligned_ptr = unsafe { raw.as_mut_ptr().add(offset) };
                // SAFETY: `msg_read_batch_non_pod_impl` writes `out_count` pointers with provenance via
                // `write_unaligned` into `unaligned_ptr`.
                let rc = unsafe {
                    msg_read_batch_non_pod_impl::<NonPodItem>(
                        cell,
                        msg_res_id,
                        cursor.as_mut(),
                        unaligned_ptr,
                        batch_size,
                        &raw mut out_count,
                    )
                };
                assert_eq!(rc, FfiResult::OK);
                if out_count == 0 {
                    break;
                }
                for i in 0..out_count {
                    // SAFETY: `read_unaligned` reconstructs the exact `*const c_void` with provenance
                    // written into `unaligned_ptr + i * ptr_size`, pointing to a live `NonPodItem`.
                    let p = unsafe {
                        core::ptr::read_unaligned(
                            unaligned_ptr.add(i * ptr_size).cast::<*const c_void>(),
                        )
                    };
                    // SAFETY: `Messages<NonPodItem>` remains borrowed via `cell` without mutation.
                    let item_ref = unsafe { &*p.cast::<NonPodItem>() };
                    collected.push(item_ref.clone());
                }
            }
            assert_eq!(collected, sent);
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{clamp_batch_max_count, drain_cursor_bounded};

    struct NextAdvancesCursor {
        pos: usize,
        total: usize,
    }

    impl Iterator for NextAdvancesCursor {
        type Item = usize;

        fn next(&mut self) -> Option<Self::Item> {
            if self.pos >= self.total {
                return None;
            }
            let item = self.pos;
            self.pos += 1;
            Some(item)
        }
    }

    #[kani::proof]
    #[kani::unwind(19)]
    fn kani_proof_drain_cursor_bounded_conservation() {
        let total: usize = kani::any();
        kani::assume(total <= 16);
        let max_count: usize = kani::any();
        kani::assume(max_count <= 16);

        let mut cursor = NextAdvancesCursor { pos: 0, total };
        let mut out = [0usize; 16];
        let mut collected = 0usize;

        loop {
            let base = collected;
            let n = drain_cursor_bounded(&mut cursor, max_count, |idx, item| {
                out[base + idx] = item;
            });
            if n == 0 {
                break;
            }
            collected += n;
        }

        if max_count == 0 {
            assert!(collected == 0 && cursor.pos == 0);
        } else {
            assert!(collected == total && cursor.pos == total);
            let mut i = 0usize;
            while i < total {
                assert!(out[i] == i);
                i += 1;
            }
        }
    }

    #[kani::proof]
    fn kani_proof_clamp_batch_max_count_no_isize_overflow() {
        let max_count: usize = kani::any();
        let elem_size: usize = kani::any();
        kani::assume(
            elem_size <= 64
                || matches!(elem_size, 128 | 256 | 512 | 1024 | 4096)
                || elem_size == (isize::MAX as usize) - 1
                || elem_size == isize::MAX as usize
                || elem_size == (isize::MAX as usize) + 1
                || elem_size == usize::MAX,
        );
        let eff = clamp_batch_max_count(max_count, elem_size);
        assert!(eff <= max_count);
        if elem_size > 0 {
            assert!(
                eff.checked_mul(elem_size)
                    .is_some_and(|bytes| bytes <= isize::MAX as usize)
            );
        }
    }
}
