//! Rigorous conservation-law integration & stateful `proptest` differential oracle tests
//! for `host_msg_read_batch` (POD, ZST, over-aligned SIMD, non-POD) and `host_iter_read_removed`.

use bamabi_host::prelude::*;
use bamabi_host_sys::{
    BamQuerySpecOwned, GuestSystem, host_iter_read_removed, host_msg_read_batch, host_sys_query,
};
use bamabi_sys::{
    AccessMode, BamIter, BamSystemCtx, BamTermDesc, FfiEntity, FfiResult, FfiSlice,
    ModComponent as _, StrView,
};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::system::System as BevySystem;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use proptest::prelude::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "cursor_cons::SeqMsg")]
struct SeqMsg {
    seq: u64,
    tag: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "cursor_cons::ZstMsg")]
struct ZstMsg;

#[repr(C, align(16))]
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "cursor_cons::SimdMsg")]
struct SimdMsg {
    lanes: [u32; 4],
}

#[derive(Component, Clone, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "cursor_cons::ChatMsg")]
struct ChatMsg {
    sender: String,
    tags: Vec<u32>,
    seq: u64,
}

impl Message for ChatMsg {}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "cursor_cons::TrackedComp")]
struct TrackedComp {
    id: u32,
}

struct PodReaderState {
    max_count: AtomicUsize,
    buf_elems: AtomicUsize,
    elem_size: usize,
    offset: usize,
    max_batches: AtomicUsize,
    total_count: AtomicUsize,
    trailing_zero_verified: AtomicBool,
    received_bytes: Mutex<Vec<u8>>,
}

impl PodReaderState {
    fn new(
        max_count: usize,
        buf_elems: usize,
        elem_size: usize,
        offset: usize,
        max_batches: usize,
    ) -> Self {
        Self {
            max_count: AtomicUsize::new(max_count),
            buf_elems: AtomicUsize::new(buf_elems),
            elem_size,
            offset,
            max_batches: AtomicUsize::new(max_batches),
            total_count: AtomicUsize::new(0),
            trailing_zero_verified: AtomicBool::new(false),
            received_bytes: Mutex::new(Vec::new()),
        }
    }
}

unsafe extern "C" fn pod_msg_batch_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let state = unsafe { &*user_data.cast::<PodReaderState>() };
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }

    let max_count = state.max_count.load(Ordering::SeqCst);
    let buf_elems = state.buf_elems.load(Ordering::SeqCst);
    let max_batches = state.max_batches.load(Ordering::SeqCst);
    let elem_size = state.elem_size;

    let mut scratch = if elem_size == 0 {
        Vec::new()
    } else {
        vec![0u8; buf_elems * elem_size + state.offset + 16]
    };
    let dst = if elem_size == 0 {
        core::ptr::null_mut()
    } else {
        unsafe { scratch.as_mut_ptr().add(state.offset) }
    };

    let mut batches_run = 0usize;
    while batches_run < max_batches {
        let mut read_count = usize::MAX;
        let rc = unsafe { host_msg_read_batch(iter, 0, dst, max_count, &mut read_count) };
        if rc.is_err() {
            return rc;
        }
        assert!(read_count <= max_count);
        if elem_size > 0 {
            assert!(read_count <= buf_elems);
        }
        if read_count == 0 {
            state.trailing_zero_verified.store(true, Ordering::SeqCst);
            break;
        }
        state.total_count.fetch_add(read_count, Ordering::SeqCst);
        if elem_size > 0 {
            state
                .received_bytes
                .lock()
                .unwrap()
                .extend_from_slice(&scratch[state.offset..state.offset + read_count * elem_size]);
        }
        batches_run += 1;
    }

    FfiResult::OK
}

fn decode_pod_vec<M: Copy>(bytes: &[u8]) -> Vec<M> {
    let elem_size = core::mem::size_of::<M>();
    assert_eq!(bytes.len() % elem_size, 0);
    let count = bytes.len() / elem_size;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let item =
            unsafe { core::ptr::read_unaligned(bytes.as_ptr().add(i * elem_size).cast::<M>()) };
        out.push(item);
    }
    out
}

fn run_pod_drain<M: Copy>(
    app: &mut App,
    msg_h: bamabi_sys::BamComponent,
    sys_name: &str,
    max_count: usize,
    buf_elems: usize,
    offset: usize,
) -> Vec<M> {
    let reg = app.world().resource::<ModRegistry>().clone();
    let state = Arc::new(PodReaderState::new(
        max_count,
        buf_elems,
        core::mem::size_of::<M>(),
        offset,
        usize::MAX,
    ));
    let mut sys = GuestSystem::new(
        sys_name,
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            msg_h,
            AccessMode::MESSAGE_READ,
        )])],
        pod_msg_batch_cb,
        Arc::as_ptr(&state) as *mut c_void,
        reg,
    );
    sys.initialize(app.world_mut());
    let _ = sys.run((), app.world_mut());
    assert!(
        state.trailing_zero_verified.load(Ordering::SeqCst),
        "trailing read after drain must return 0 for {sys_name}"
    );
    let bytes = state.received_bytes.lock().unwrap();
    decode_pod_vec::<M>(&bytes)
}

#[test]
fn pod_messages_boundary_sweep_conserves_all_items() {
    const N_CASES: [usize; 11] = [0, 1, 63, 64, 65, 127, 128, 129, 192, 193, 1000];
    const MAX_COUNTS: [usize; 9] = [1, 2, 3, 7, 63, 64, 65, 128, usize::MAX];

    for &n in &N_CASES {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let seq_h = app.register_mod_message::<SeqMsg>().unwrap();
        let zst_h = app.register_mod_message::<ZstMsg>().unwrap();
        let simd_h = app.register_mod_message::<SimdMsg>().unwrap();

        let sent_seq: Vec<SeqMsg> = (0..n)
            .map(|i| SeqMsg {
                seq: i as u64,
                tag: (i as u32).wrapping_mul(29),
                pad: 0xDEAD_BEEF,
            })
            .collect();
        let sent_simd: Vec<SimdMsg> = (0..n)
            .map(|i| {
                let v = i as u32;
                SimdMsg {
                    lanes: [v, v.wrapping_add(1), v.wrapping_add(2), v.wrapping_add(3)],
                }
            })
            .collect();

        {
            let mut msgs = app.world_mut().resource_mut::<Messages<SeqMsg>>();
            for &m in &sent_seq {
                msgs.write(m);
            }
        }
        {
            let mut msgs = app.world_mut().resource_mut::<Messages<ZstMsg>>();
            for _ in 0..n {
                msgs.write(ZstMsg);
            }
        }
        {
            let mut msgs = app.world_mut().resource_mut::<Messages<SimdMsg>>();
            for &m in &sent_simd {
                msgs.write(m);
            }
        }

        for (mc_idx, &max_count) in MAX_COUNTS.iter().enumerate() {
            let buf_elems = if max_count == usize::MAX {
                n.max(1)
            } else {
                max_count
            };

            let got_seq = run_pod_drain::<SeqMsg>(
                &mut app,
                seq_h,
                &format!("seq_{n}_{mc_idx}"),
                max_count,
                buf_elems,
                0,
            );
            assert_eq!(
                got_seq, sent_seq,
                "SeqMsg mismatch at N={n}, max={max_count}"
            );

            // ZST messages with explicit null `out_buf`.
            let reg = app.world().resource::<ModRegistry>().clone();
            let zst_state = Arc::new(PodReaderState::new(max_count, 0, 0, 0, usize::MAX));
            let mut zst_sys = GuestSystem::new(
                format!("zst_{n}_{mc_idx}"),
                vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                    zst_h,
                    AccessMode::MESSAGE_READ,
                )])],
                pod_msg_batch_cb,
                Arc::as_ptr(&zst_state) as *mut c_void,
                reg,
            );
            zst_sys.initialize(app.world_mut());
            let _ = zst_sys.run((), app.world_mut());
            assert!(zst_state.trailing_zero_verified.load(Ordering::SeqCst));
            assert_eq!(
                zst_state.total_count.load(Ordering::SeqCst),
                n,
                "ZstMsg count mismatch at N={n}, max={max_count}"
            );

            // Over-aligned `SimdMsg` (`align(16)`) with unaligned destination offset `+1..=+8`.
            let offset = (mc_idx % 8) + 1;
            let got_simd = run_pod_drain::<SimdMsg>(
                &mut app,
                simd_h,
                &format!("simd_{n}_{mc_idx}"),
                max_count,
                buf_elems,
                offset,
            );
            assert_eq!(
                got_simd, sent_simd,
                "SimdMsg mismatch at N={n}, max={max_count}, offset={offset}"
            );
        }
    }

    // Explicit exhaustive unaligned offset `+1..=+8` check on boundary counts.
    for &n in &[1usize, 64, 65, 129] {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let simd_h = app.register_mod_message::<SimdMsg>().unwrap();
        let sent_simd: Vec<SimdMsg> = (0..n)
            .map(|i| SimdMsg {
                lanes: [i as u32, (i * 2) as u32, (i * 3) as u32, (i * 4) as u32],
            })
            .collect();
        {
            let mut msgs = app.world_mut().resource_mut::<Messages<SimdMsg>>();
            for &m in &sent_simd {
                msgs.write(m);
            }
        }
        for &max_count in &[1usize, 7, 64, 65, usize::MAX] {
            let buf_elems = if max_count == usize::MAX {
                n
            } else {
                max_count
            };
            for offset in 1usize..=8 {
                let got = run_pod_drain::<SimdMsg>(
                    &mut app,
                    simd_h,
                    &format!("simd_off_{n}_{max_count}_{offset}"),
                    max_count,
                    buf_elems,
                    offset,
                );
                assert_eq!(
                    got, sent_simd,
                    "SimdMsg unaligned offset={offset} failed at N={n}, max={max_count}"
                );
            }
        }
    }
}

#[test]
fn multi_reader_rollover_and_hot_reload_cursor_persistence() {
    // Part A: Independent readers across partial reads + `Messages::update()` rollover + burst 2.
    {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let seq_h = app.register_mod_message::<SeqMsg>().unwrap();
        let reg = app.world().resource::<ModRegistry>().clone();

        let burst1: Vec<SeqMsg> = (0..90u64)
            .map(|seq| SeqMsg {
                seq,
                tag: 1,
                pad: 0,
            })
            .collect();
        let burst2: Vec<SeqMsg> = (90..165u64)
            .map(|seq| SeqMsg {
                seq,
                tag: 2,
                pad: 0,
            })
            .collect();
        let mut expected_all = burst1.clone();
        expected_all.extend_from_slice(&burst2);

        {
            let mut msgs = app.world_mut().resource_mut::<Messages<SeqMsg>>();
            for &m in &burst1 {
                msgs.write(m);
            }
        }

        let elem_size = core::mem::size_of::<SeqMsg>();
        // Reader A reads 3 batches of 7 (21 msgs) before rollover; Reader B reads 1 batch of 64 (64 msgs).
        let state_a = Arc::new(PodReaderState::new(7, 7, elem_size, 0, 3));
        let state_b = Arc::new(PodReaderState::new(64, 64, elem_size, 0, 1));

        let mut sys_a = GuestSystem::new(
            "reader_a",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                seq_h,
                AccessMode::MESSAGE_READ,
            )])],
            pod_msg_batch_cb,
            Arc::as_ptr(&state_a) as *mut c_void,
            reg.clone(),
        );
        let mut sys_b = GuestSystem::new(
            "reader_b",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                seq_h,
                AccessMode::MESSAGE_READ,
            )])],
            pod_msg_batch_cb,
            Arc::as_ptr(&state_b) as *mut c_void,
            reg,
        );

        sys_a.initialize(app.world_mut());
        sys_b.initialize(app.world_mut());

        let _ = sys_a.run((), app.world_mut());
        let _ = sys_b.run((), app.world_mut());
        assert_eq!(
            decode_pod_vec::<SeqMsg>(&state_a.received_bytes.lock().unwrap()).len(),
            21
        );
        assert_eq!(
            decode_pod_vec::<SeqMsg>(&state_b.received_bytes.lock().unwrap()).len(),
            64
        );

        // Single double-buffer rollover + second burst.
        app.world_mut().resource_mut::<Messages<SeqMsg>>().update();
        {
            let mut msgs = app.world_mut().resource_mut::<Messages<SeqMsg>>();
            for &m in &burst2 {
                msgs.write(m);
            }
        }

        // Drain both readers to completion.
        state_a.max_batches.store(usize::MAX, Ordering::SeqCst);
        state_b.max_batches.store(usize::MAX, Ordering::SeqCst);
        let _ = sys_a.run((), app.world_mut());
        let _ = sys_b.run((), app.world_mut());

        assert!(state_a.trailing_zero_verified.load(Ordering::SeqCst));
        assert!(state_b.trailing_zero_verified.load(Ordering::SeqCst));
        assert_eq!(
            decode_pod_vec::<SeqMsg>(&state_a.received_bytes.lock().unwrap()),
            expected_all
        );
        assert_eq!(
            decode_pod_vec::<SeqMsg>(&state_b.received_bytes.lock().unwrap()),
            expected_all
        );
    }

    // Part B: Hot-reload cursor persistence via `PersistentMessageCursors`.
    {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let seq_h = app.register_mod_message::<SeqMsg>().unwrap();
        let reg = app.world().resource::<ModRegistry>().clone();

        let total = 150usize;
        let sent: Vec<SeqMsg> = (0..total)
            .map(|i| SeqMsg {
                seq: i as u64,
                tag: (i as u32) + 7,
                pad: 0xCAFE,
            })
            .collect();
        {
            let mut msgs = app.world_mut().resource_mut::<Messages<SeqMsg>>();
            for &m in &sent {
                msgs.write(m);
            }
        }

        let elem_size = core::mem::size_of::<SeqMsg>();
        let v1_state = Arc::new(PodReaderState::new(13, 13, elem_size, 0, 5));
        let query_spec = vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            seq_h,
            AccessMode::MESSAGE_READ,
        )])];

        let mut sys_v1 = GuestSystem::new(
            "reload_reader",
            query_spec.clone(),
            pod_msg_batch_cb,
            Arc::as_ptr(&v1_state) as *mut c_void,
            reg.clone(),
        )
        .with_mod_namespace("hot_reload_mod");
        sys_v1.initialize(app.world_mut());
        let _ = sys_v1.run((), app.world_mut());

        let before_reload = decode_pod_vec::<SeqMsg>(&v1_state.received_bytes.lock().unwrap());
        assert_eq!(before_reload.len(), 65);
        assert_eq!(before_reload, sent[..65]);

        // Dropping `sys_v1` persists its `MessageCursor<SeqMsg>` into `PersistentMessageCursors`.
        drop(sys_v1);

        let v2_state = Arc::new(PodReaderState::new(17, 17, elem_size, 0, usize::MAX));
        let mut sys_v2 = GuestSystem::new(
            "reload_reader",
            query_spec,
            pod_msg_batch_cb,
            Arc::as_ptr(&v2_state) as *mut c_void,
            reg,
        )
        .with_mod_namespace("hot_reload_mod");
        sys_v2.initialize(app.world_mut());
        let _ = sys_v2.run((), app.world_mut());

        assert!(v2_state.trailing_zero_verified.load(Ordering::SeqCst));
        let after_reload = decode_pod_vec::<SeqMsg>(&v2_state.received_bytes.lock().unwrap());
        assert_eq!(after_reload.len(), 85);

        let mut combined = before_reload;
        combined.extend_from_slice(&after_reload);
        assert_eq!(combined, sent);
    }
}

fn decode_host_chat_msg(host_ptr: *const c_void) -> ChatMsg {
    type ExportFieldsFn = unsafe extern "C" fn(
        src_ptr: *const c_void,
        sink: Option<
            unsafe extern "C" fn(
                sink_ctx: *mut c_void,
                field_name: *const StrView,
                type_tag: *const StrView,
                payload: *const FfiSlice<u8>,
            ) -> FfiResult,
        >,
        sink_ctx: *mut c_void,
    ) -> FfiResult;
    type ImportFieldFn = unsafe extern "C" fn(
        dst_ptr: *mut c_void,
        field_name: *const StrView,
        type_tag: *const StrView,
        payload: *const FfiSlice<u8>,
    ) -> FfiResult;

    let export_ptr =
        ChatMsg::resolve_symbol(&format!("bam.v0.{}.export_fields", ChatMsg::CANONICAL_NAME))
            .unwrap();
    let import_ptr =
        ChatMsg::resolve_symbol(&format!("bam.v0.{}.import_field", ChatMsg::CANONICAL_NAME))
            .unwrap();
    let export_fn: ExportFieldsFn = unsafe { core::mem::transmute(export_ptr) };
    let import_fn: ImportFieldFn = unsafe { core::mem::transmute(import_ptr) };

    struct Ctx {
        dst: *mut c_void,
        import_fn: ImportFieldFn,
    }
    unsafe extern "C" fn fwd(
        ctx: *mut c_void,
        name: *const StrView,
        tag: *const StrView,
        payload: *const FfiSlice<u8>,
    ) -> FfiResult {
        let c = unsafe { &*ctx.cast::<Ctx>() };
        unsafe { (c.import_fn)(c.dst, name, tag, payload) }
    }

    let mut out = ChatMsg::default();
    let mut ctx = Ctx {
        dst: (&raw mut out).cast::<c_void>(),
        import_fn,
    };
    let rc = unsafe { export_fn(host_ptr, Some(fwd), (&raw mut ctx).cast::<c_void>()) };
    assert_eq!(rc, FfiResult::OK);
    out
}

#[test]
fn non_pod_messages_and_removed_components_conservation() {
    // Part A: Non-POD `Messages<ChatMsg>` parity across boundary batch sizes.
    {
        struct NonPodReaderState {
            batch_size: usize,
            trailing_zero_verified: AtomicBool,
            received: Mutex<Vec<ChatMsg>>,
        }

        unsafe extern "C" fn non_pod_cb(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let state = unsafe { &*user_data.cast::<NonPodReaderState>() };
            let mut iter: *mut BamIter = core::ptr::null_mut();
            let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
            if rc.is_err() {
                return rc;
            }
            let mut ptrs = vec![core::ptr::null::<c_void>(); state.batch_size];
            loop {
                let mut count = usize::MAX;
                let rc = unsafe {
                    host_msg_read_batch(
                        iter,
                        0,
                        ptrs.as_mut_ptr().cast::<u8>(),
                        state.batch_size,
                        &mut count,
                    )
                };
                if rc.is_err() {
                    return rc;
                }
                assert!(count <= state.batch_size);
                if count == 0 {
                    state.trailing_zero_verified.store(true, Ordering::SeqCst);
                    break;
                }
                let mut guard = state.received.lock().unwrap();
                for &p in &ptrs[..count] {
                    guard.push(decode_host_chat_msg(p));
                }
            }
            FfiResult::OK
        }

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.world_mut().init_resource::<Messages<ChatMsg>>();
        let msg_res_id = app
            .world()
            .components()
            .component_id::<Messages<ChatMsg>>()
            .unwrap();
        let chat_h = app
            .world_mut()
            .resource_mut::<ModRegistry>()
            .register_non_pod_message::<ChatMsg>(msg_res_id)
            .unwrap();

        let total = 135usize;
        let sent: Vec<ChatMsg> = (0..total)
            .map(|i| ChatMsg {
                sender: format!("user_{i}"),
                tags: vec![i as u32, (i * 2) as u32],
                seq: i as u64,
            })
            .collect();
        {
            let mut msgs = app.world_mut().resource_mut::<Messages<ChatMsg>>();
            for item in &sent {
                msgs.write(item.clone());
            }
        }

        for batch_size in [1usize, 2, 7, 64, 65] {
            let reg = app.world().resource::<ModRegistry>().clone();
            let state = Arc::new(NonPodReaderState {
                batch_size,
                trailing_zero_verified: AtomicBool::new(false),
                received: Mutex::new(Vec::new()),
            });
            let mut sys = GuestSystem::new(
                format!("non_pod_{batch_size}"),
                vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                    chat_h,
                    AccessMode::MESSAGE_READ,
                )])],
                non_pod_cb,
                Arc::as_ptr(&state) as *mut c_void,
                reg,
            );
            sys.initialize(app.world_mut());
            let _ = sys.run((), app.world_mut());
            assert!(state.trailing_zero_verified.load(Ordering::SeqCst));
            assert_eq!(
                *state.received.lock().unwrap(),
                sent,
                "Non-POD ChatMsg conservation failed for batch_size={batch_size}"
            );
        }
    }

    // Part B: `RemovedComponents<TrackedComp>` parity across boundary batch sizes.
    {
        struct RemovedReaderState {
            batch_size: usize,
            trailing_zero_verified: AtomicBool,
            received: Mutex<Vec<Entity>>,
        }

        unsafe extern "C" fn removed_cb(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let state = unsafe { &*user_data.cast::<RemovedReaderState>() };
            let mut iter: *mut BamIter = core::ptr::null_mut();
            let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
            if rc.is_err() {
                return rc;
            }
            let mut buf = vec![FfiEntity::NULL; state.batch_size];
            loop {
                let mut count = usize::MAX;
                let rc = unsafe {
                    host_iter_read_removed(iter, 0, buf.as_mut_ptr(), state.batch_size, &mut count)
                };
                if rc.is_err() {
                    return rc;
                }
                assert!(count <= state.batch_size);
                if count == 0 {
                    state.trailing_zero_verified.store(true, Ordering::SeqCst);
                    break;
                }
                let mut guard = state.received.lock().unwrap();
                for &ffi_e in &buf[..count] {
                    guard.push(Entity::try_from_bits(ffi_e.to_bits()).unwrap());
                }
            }
            FfiResult::OK
        }

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let tracked_h = app
            .register_mod_component::<TrackedComp>(ComponentOptions::new())
            .unwrap();

        let total = 135usize;
        let mut spawned = Vec::with_capacity(total);
        for i in 0..total {
            spawned.push(app.world_mut().spawn(TrackedComp { id: i as u32 }).id());
        }
        for &e in &spawned {
            app.world_mut().entity_mut(e).remove::<TrackedComp>();
        }

        for batch_size in [1usize, 2, 7, 64, 65] {
            let reg = app.world().resource::<ModRegistry>().clone();
            let state = Arc::new(RemovedReaderState {
                batch_size,
                trailing_zero_verified: AtomicBool::new(false),
                received: Mutex::new(Vec::new()),
            });
            let mut sys = GuestSystem::new(
                format!("removed_{batch_size}"),
                vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                    tracked_h,
                    AccessMode::REMOVED_READ,
                )])],
                removed_cb,
                Arc::as_ptr(&state) as *mut c_void,
                reg,
            );
            sys.initialize(app.world_mut());
            let _ = sys.run((), app.world_mut());
            assert!(state.trailing_zero_verified.load(Ordering::SeqCst));
            assert_eq!(
                *state.received.lock().unwrap(),
                spawned,
                "RemovedComponents conservation failed for batch_size={batch_size}"
            );
        }
    }
}

#[derive(Clone, Debug)]
enum OracleOp {
    WriteBurst(usize),
    ReaderARead(usize),
    ReaderBRead(usize),
    WorldUpdateRollover,
}

fn batch_size_strategy() -> impl Strategy<Value = usize> {
    prop_oneof![1..=8usize, 63..=66usize, 127..=130usize]
}

fn oracle_op_strategy() -> impl Strategy<Value = OracleOp> {
    prop_oneof![
        3 => (0usize..=80).prop_map(OracleOp::WriteBurst),
        3 => batch_size_strategy().prop_map(OracleOp::ReaderARead),
        3 => batch_size_strategy().prop_map(OracleOp::ReaderBRead),
        2 => Just(OracleOp::WorldUpdateRollover),
    ]
}

struct SingleStepReaderState {
    max_count: AtomicUsize,
    last_batch: Mutex<Vec<SeqMsg>>,
}

unsafe extern "C" fn single_step_read_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let state = unsafe { &*user_data.cast::<SingleStepReaderState>() };
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }

    let max_count = state.max_count.load(Ordering::SeqCst);
    let mut buf = vec![SeqMsg::default(); max_count.max(1)];
    let mut count = usize::MAX;
    let rc = unsafe {
        host_msg_read_batch(
            iter,
            0,
            buf.as_mut_ptr().cast::<u8>(),
            max_count,
            &mut count,
        )
    };
    if rc.is_err() {
        return rc;
    }
    assert!(count <= max_count);
    let mut out = state.last_batch.lock().unwrap();
    out.clear();
    out.extend_from_slice(&buf[..count]);
    FfiResult::OK
}

proptest! {
    #[test]
    fn proptest_multi_reader_cursor_differential_oracle(
        ops in prop::collection::vec(oracle_op_strategy(), 1..64)
    ) {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let seq_h = app.register_mod_message::<SeqMsg>().unwrap();
        let reg = app.world().resource::<ModRegistry>().clone();

        let state_a = Arc::new(SingleStepReaderState {
            max_count: AtomicUsize::new(1),
            last_batch: Mutex::new(Vec::new()),
        });
        let state_b = Arc::new(SingleStepReaderState {
            max_count: AtomicUsize::new(1),
            last_batch: Mutex::new(Vec::new()),
        });

        let mut sys_a = GuestSystem::new(
            "oracle_a",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                seq_h,
                AccessMode::MESSAGE_READ,
            )])],
            single_step_read_cb,
            Arc::as_ptr(&state_a) as *mut c_void,
            reg.clone(),
        );
        let mut sys_b = GuestSystem::new(
            "oracle_b",
            vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
                seq_h,
                AccessMode::MESSAGE_READ,
            )])],
            single_step_read_cb,
            Arc::as_ptr(&state_b) as *mut c_void,
            reg,
        );
        sys_a.initialize(app.world_mut());
        sys_b.initialize(app.world_mut());

        let mut oracle_a: VecDeque<(u64, SeqMsg)> = VecDeque::new();
        let mut oracle_b: VecDeque<(u64, SeqMsg)> = VecDeque::new();
        let mut current_frame = 0u64;
        let mut next_seq = 0u64;

        for op in ops {
            match op {
                OracleOp::WriteBurst(k) => {
                    let mut msgs = app.world_mut().resource_mut::<Messages<SeqMsg>>();
                    for _ in 0..k {
                        let msg = SeqMsg {
                            seq: next_seq,
                            tag: (next_seq as u32).wrapping_mul(17),
                            pad: (current_frame as u32),
                        };
                        next_seq += 1;
                        msgs.write(msg);
                        oracle_a.push_back((current_frame, msg));
                        oracle_b.push_back((current_frame, msg));
                    }
                }
                OracleOp::WorldUpdateRollover => {
                    app.world_mut().resource_mut::<Messages<SeqMsg>>().update();
                    current_frame += 1;
                    while oracle_a.front().is_some_and(|(wf, _)| *wf + 2 <= current_frame) {
                        oracle_a.pop_front();
                    }
                    while oracle_b.front().is_some_and(|(wf, _)| *wf + 2 <= current_frame) {
                        oracle_b.pop_front();
                    }
                }
                OracleOp::ReaderARead(b) => {
                    state_a.max_count.store(b, Ordering::SeqCst);
                    let _ = sys_a.run((), app.world_mut());
                    let actual = state_a.last_batch.lock().unwrap().clone();
                    let expected_len = b.min(oracle_a.len());
                    prop_assert_eq!(actual.len(), expected_len);
                    for got in actual {
                        let (_, expected) = oracle_a.pop_front().unwrap();
                        prop_assert_eq!(got, expected);
                    }
                }
                OracleOp::ReaderBRead(b) => {
                    state_b.max_count.store(b, Ordering::SeqCst);
                    let _ = sys_b.run((), app.world_mut());
                    let actual = state_b.last_batch.lock().unwrap().clone();
                    let expected_len = b.min(oracle_b.len());
                    prop_assert_eq!(actual.len(), expected_len);
                    for got in actual {
                        let (_, expected) = oracle_b.pop_front().unwrap();
                        prop_assert_eq!(got, expected);
                    }
                }
            }
        }

        // Final boundary drain verifying complete tail parity for both readers.
        for (sys, state, oracle) in [
            (&mut sys_a, &state_a, &mut oracle_a),
            (&mut sys_b, &state_b, &mut oracle_b),
        ] {
            state.max_count.store(7, Ordering::SeqCst);
            while !oracle.is_empty() {
                let _ = sys.run((), app.world_mut());
                let actual = state.last_batch.lock().unwrap().clone();
                let expected_len = 7usize.min(oracle.len());
                prop_assert_eq!(actual.len(), expected_len);
                for got in actual {
                    let (_, expected) = oracle.pop_front().unwrap();
                    prop_assert_eq!(got, expected);
                }
            }
            let _ = sys.run((), app.world_mut());
            prop_assert!(state.last_batch.lock().unwrap().is_empty());
        }
    }
}
