//! ABI `0.3.0` Subsystem Benchmark Suite.
//!
//! Exercises the new ABI `0.3.0` host slots through `bamabi_host_sys` using the same
//! zero-dependency harness conventions as `chunk_throughput.rs` (`--bench`, `--test`, `--quick`,
//! and substring filter arguments supported):
//!
//! 1. **`relations`** — `cmd_insert_relation` / `cmd_remove_relation` (`10k` edges) and
//!    `CASCADE_DELETE` despawn of a `10k`-node chain.
//! 2. **`intern_symbol`** — `host_intern_symbol` (`10k` unique strings, then `100k` repeated lookups).
//! 3. **`iter_count`** — `host_iter_count` vs full `host_iter_next_chunk` count at `10k` / `100k`.
//! 4. **`checkpoint_rollback`** — `10k` commands with `cmd_checkpoint` + `cmd_rollback`.
//! 5. **`migrate_column`** — `migrate_component_column` (`8B -> 16B`) over `10k` entities.
//! 6. **`par_readonly`** — `par_for_each_chunk_readonly` vs sequential chunk iteration at `100k`.
//! 7. **`up_cascade`** — `iter_up_cascade` on a depth-`64` hierarchy x `1k` leaves.
//! 8. **`schedule_set_rewire`** — `configure_schedule_set` + `mount_system_in_schedule` build
//!    and `rewire_mod_schedule_sets_in_world` hot-rewire rebuild cost.

#![deny(unsafe_op_in_unsafe_fn)]

use bamabi_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
use bamabi_host_sys::loader::{BamSymbolTable, ensure_symbol_tables_in_world};
use bamabi_host_sys::registry::catalog::ModRelationGraph;
use bamabi_host_sys::schedule::{
    ModScheduleSetRegistry, mount_mod_systems_in_world, rewire_mod_schedule_sets_in_world,
};
use bamabi_host_sys::{
    BamQuerySpecOwned, GuestSystem, HOST_API, HostCommands, HostCommandsStorage, HostContext,
    HostIter, LoadContext, ModHost, ModId, ModScheduleMap, QueryPlan, QueryTermMeta,
    WorldDropGuard, host_iter_column_readonly, host_iter_entities, host_iter_next_chunk,
    host_iter_reset, host_register_component, host_register_relation, host_res_ptr, host_sys_query,
};
use bamabi_sys::{
    AccessMode, BAM_RELATION_FLAG_CASCADE_DELETE, BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_SCHEDULE_SET_FLAG_SEQUENCE, BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK, BamComponent,
    BamFieldDesc, BamFieldKind, BamIter, BamRelationDesc, BamScheduleSetDesc, BamSymbol, BamSystem,
    BamSystemCtx, BamSystemDesc, BamSystemSet, BamTermDesc, ComponentDesc, ComponentUuid, FfiBool,
    FfiEntity, FfiResult, ModComponent as _, ScheduleId, StrView,
};
use bevy_app::{App, Update};
use bevy_ecs::change_detection::Tick;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::schedule::{Schedule, Schedules};
use bevy_ecs::storage::TableId;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::ptr::{null, null_mut};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::env;
use std::ffi::CString;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ============================================================================
// Benchmark ECS Components & Resources
// ============================================================================

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "v03_bench::Position")]
struct Position {
    x: f32,
    y: f32,
    z: f32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "v03_bench::LocalOffset")]
struct LocalOffset {
    dx: i64,
    dy: i64,
    dz: i64,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "v03_bench::OptBonus")]
struct OptBonus {
    bonus: i64,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default, ModComponent)]
#[mod_component(name = "v03_bench::FilterTagA")]
struct FilterTagA;

#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "v03_bench::ScaleRes")]
struct ScaleRes {
    factor: i64,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "v03_bench::ThemeColor")]
struct ThemeColor {
    rgba: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "v03_bench::LeafMarker")]
struct LeafMarker {
    idx: u32,
}

#[derive(Component, Default, Clone, Copy)]
struct TableBit0;
#[derive(Component, Default, Clone, Copy)]
struct TableBit1;
#[derive(Component, Default, Clone, Copy)]
struct TableBit2;
#[derive(Component, Default, Clone, Copy)]
struct TableBit3;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CoordV1 {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CoordV2 {
    x: f64,
    y: f64,
}

// ============================================================================
// CLI Mode Parsing & Timing Harness
// ============================================================================

#[derive(Clone, Debug)]
struct BenchConfig {
    test_mode: bool,
    quick_mode: bool,
    warmup: usize,
    rounds: usize,
    filter: Option<String>,
}

impl BenchConfig {
    fn from_args() -> Self {
        let mut test_mode = false;
        let mut quick_mode = false;
        let mut filter: Option<String> = None;

        for arg in env::args().skip(1) {
            match arg.as_str() {
                "--test" => test_mode = true,
                "--quick" => quick_mode = true,
                "--bench" | "--nocapture" => {}
                other if !other.starts_with('-') => {
                    filter = Some(other.to_owned());
                }
                _ => {}
            }
        }

        let (warmup, rounds) = if test_mode {
            (0, 1)
        } else if quick_mode {
            (3, 12)
        } else {
            (5, 30)
        };

        Self {
            test_mode,
            quick_mode,
            warmup,
            rounds,
            filter,
        }
    }

    fn matches(&self, id: &str) -> bool {
        match &self.filter {
            Some(pat) => id.contains(pat.as_str()),
            None => true,
        }
    }

    fn scale_count(&self, full: usize) -> usize {
        if self.test_mode { full.min(256) } else { full }
    }
}

#[derive(Clone, Copy, Debug)]
struct Stats {
    median_ms: f64,
    p95_ms: f64,
    ops_per_sec: f64,
    ns_per_op: f64,
}

#[inline]
fn timed_update(app: &mut App) -> Duration {
    let start = Instant::now();
    app.update();
    start.elapsed()
}

fn measure_fn<F: FnMut() -> Duration>(
    mut f: F,
    warmup: usize,
    rounds: usize,
    ops_per_iter: usize,
) -> Stats {
    for _ in 0..warmup {
        let _ = f();
    }

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        samples.push(f());
    }

    samples.sort();
    let median_ms = samples[rounds / 2].as_secs_f64() * 1000.0;
    let p95_idx = ((rounds * 95) / 100).min(rounds.saturating_sub(1));
    let p95_ms = samples[p95_idx].as_secs_f64() * 1000.0;
    let total: Duration = samples.iter().sum();
    let avg_secs = total.as_secs_f64() / (rounds as f64);
    let ops = ops_per_iter.max(1) as f64;
    let ops_per_sec = ops / avg_secs.max(1e-12);
    let ns_per_op = (median_ms * 1_000_000.0) / ops;

    Stats {
        median_ms,
        p95_ms,
        ops_per_sec,
        ns_per_op,
    }
}

fn print_row(label: &str, stats: Stats, unit: &str) {
    println!(
        "  {:<42} | median {:>8.4} ms | p95 {:>8.4} ms | {:>13.0} {unit}/s | {:>6.2} ns/{unit}",
        label, stats.median_ms, stats.p95_ms, stats.ops_per_sec, stats.ns_per_op
    );
}

#[inline]
fn ffi_ent(e: Entity) -> FfiEntity {
    FfiEntity::from_bits(e.to_bits())
}

// ============================================================================
// Scenario 1: `relations` (`cmd_insert_relation`, `cmd_remove_relation`, cascade despawn)
// ============================================================================

fn register_bench_relation(
    world: &mut World,
    name: &str,
    flags: u32,
    data_size: usize,
    data_align: usize,
) -> BamComponent {
    let reg = world.resource::<ModRegistry>().clone();
    let cname = CString::new(name).expect("valid C relation name");
    let uuid = ComponentUuid::from_canonical_name(name);
    let desc = BamRelationDesc::new(cname.as_ptr(), uuid)
        .with_flags(flags)
        .with_payload_layout(data_size, data_align);
    let mut load_ctx = LoadContext::with_manifest_name(
        reg,
        Arc::new(AtomicBool::new(true)),
        (&raw mut *world).cast::<World>(),
        Some(ModId(1)),
        Some(Arc::from("bench_rel")),
    );
    let mut out = BamComponent::INVALID;
    // SAFETY: `load_ctx`, `desc`, and `out` are valid aligned stack values.
    let rc = unsafe { host_register_relation(load_ctx.as_ffi_ptr(), &desc, &raw mut out) };
    assert_eq!(rc, FfiResult::OK);
    let updated_reg = load_ctx.registry.clone();
    load_ctx.finish_init();
    world.insert_resource(updated_reg);
    out
}

fn bench_relations(cfg: &BenchConfig) {
    let count = cfg.scale_count(10_000);
    println!("\n================================================================================");
    println!(" SCENARIO 1: Relations (cmd_insert/remove {count} edges & {count}-node cascade)");
    println!("================================================================================");

    let insert_fn = HOST_API
        .cmd_insert_relation
        .expect("HOST_API.cmd_insert_relation must be wired");
    let remove_fn = HOST_API
        .cmd_remove_relation
        .expect("HOST_API.cmd_remove_relation must be wired");

    // 1a & 1b. `cmd_insert_relation` and `cmd_remove_relation` over `count` edges
    {
        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModRegistry::new());
        let link_rel = register_bench_relation(
            &mut world,
            "bench_rel::LinkEdge",
            BAM_RELATION_FLAG_CLEANUP_REMOVE,
            4,
            4,
        );
        let entities: Vec<Entity> = (0..=count).map(|_| world.spawn_empty().id()).collect();
        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let payload: u32 = 42;

        let insert_stats = measure_fn(
            || {
                let start = Instant::now();
                {
                    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                    let ptr = cmds.as_ffi_ptr();
                    for i in 0..count {
                        // SAFETY: `ptr` is an active `HostCommands`, entities are live, and
                        // `payload` points to 4 readable bytes matching `link_rel`.
                        let rc = unsafe {
                            insert_fn(
                                ptr,
                                ffi_ent(entities[i]),
                                link_rel,
                                ffi_ent(entities[i + 1]),
                                (&raw const payload).cast::<c_void>(),
                                4,
                            )
                        };
                        debug_assert_eq!(rc, FfiResult::OK);
                    }
                }
                storage.apply_to_world(&mut world, &snap);
                let elapsed = start.elapsed();

                assert_eq!(
                    world.resource::<ModRelationGraph>().edge_count(link_rel),
                    count
                );

                // Clean up untimed before the next round.
                {
                    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                    let ptr = cmds.as_ffi_ptr();
                    for i in 0..count {
                        // SAFETY: Valid `cmds` and live entity handles.
                        let rc = unsafe {
                            remove_fn(
                                ptr,
                                ffi_ent(entities[i]),
                                link_rel,
                                ffi_ent(entities[i + 1]),
                            )
                        };
                        debug_assert_eq!(rc, FfiResult::OK);
                    }
                }
                storage.apply_to_world(&mut world, &snap);
                world.clear_trackers();
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            count,
        );
        print_row(
            &format!("cmd_insert_relation + flush ({count})"),
            insert_stats,
            "edge",
        );

        let remove_stats = measure_fn(
            || {
                // Pre-populate edges untimed.
                {
                    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                    let ptr = cmds.as_ffi_ptr();
                    for i in 0..count {
                        // SAFETY: Valid `cmds`, live entities, and 4-byte `payload`.
                        let rc = unsafe {
                            insert_fn(
                                ptr,
                                ffi_ent(entities[i]),
                                link_rel,
                                ffi_ent(entities[i + 1]),
                                (&raw const payload).cast::<c_void>(),
                                4,
                            )
                        };
                        debug_assert_eq!(rc, FfiResult::OK);
                    }
                }
                storage.apply_to_world(&mut world, &snap);

                let start = Instant::now();
                {
                    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                    let ptr = cmds.as_ffi_ptr();
                    for i in 0..count {
                        // SAFETY: Valid `cmds` and live entity handles.
                        let rc = unsafe {
                            remove_fn(
                                ptr,
                                ffi_ent(entities[i]),
                                link_rel,
                                ffi_ent(entities[i + 1]),
                            )
                        };
                        debug_assert_eq!(rc, FfiResult::OK);
                    }
                }
                storage.apply_to_world(&mut world, &snap);
                let elapsed = start.elapsed();

                assert_eq!(world.resource::<ModRelationGraph>().edge_count(link_rel), 0);
                world.clear_trackers();
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            count,
        );
        print_row(
            &format!("cmd_remove_relation + flush ({count})"),
            remove_stats,
            "edge",
        );
    }

    // 1c. Cascade despawn of a `count`-node chain (`BAM_RELATION_FLAG_CASCADE_DELETE`)
    {
        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModRegistry::new());
        let cascade_rel = register_bench_relation(
            &mut world,
            "bench_rel::OwnedChain",
            BAM_RELATION_FLAG_CASCADE_DELETE,
            0,
            1,
        );

        let cascade_stats = measure_fn(
            || {
                let mut chain = Vec::with_capacity(count);
                for _ in 0..count {
                    chain.push(world.spawn_empty().id());
                }
                for i in 1..count {
                    let rc = ModRelationGraph::insert_edge_in_world(
                        &mut world,
                        cascade_rel,
                        chain[i],
                        chain[i - 1],
                        &[],
                    );
                    debug_assert_eq!(rc, FfiResult::OK);
                }

                let start = Instant::now();
                let ok = world.despawn(chain[0]);
                let elapsed = start.elapsed();

                assert!(ok);
                assert_eq!(
                    world.resource::<ModRelationGraph>().edge_count(cascade_rel),
                    0
                );
                assert!(world.get_entity(chain[count - 1]).is_err());
                world.clear_trackers();
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            count,
        );
        print_row(
            &format!("Cascade despawn chain ({count} nodes)"),
            cascade_stats,
            "node",
        );
    }
}

// ============================================================================
// Scenario 2: `intern_symbol` (`10k` unique strings, then `100k` repeated lookups)
// ============================================================================

fn bench_intern_symbol(cfg: &BenchConfig) {
    let unique_count = cfg.scale_count(10_000);
    let lookup_count = if cfg.test_mode { 1_000 } else { 100_000 };
    println!("\n================================================================================");
    println!(
        " SCENARIO 2: Symbol Interner ({unique_count} unique strings, {lookup_count} lookups)"
    );
    println!("================================================================================");

    let intern_fn = HOST_API
        .intern_symbol
        .expect("HOST_API.intern_symbol must be wired");
    let strings: Vec<String> = (0..unique_count)
        .map(|i| format!("bench::sym_{i:05}"))
        .collect();

    // 2a. `unique_count` unique string interns into an empty `BamSymbolTable`
    let unique_stats = measure_fn(
        || {
            let mut world = World::new();
            world.insert_resource(WorldDropGuard::new());
            world.insert_resource(ModRegistry::new());
            let _ = ensure_symbol_tables_in_world(&mut world);
            let snap = world.resource::<ModRegistry>().snapshot_arc();
            let host_ctx = HostContext::new(snap);
            let host_ptr = host_ctx.as_ffi_ptr();

            let mut out = BamSymbol::INVALID;
            let start = Instant::now();
            for s in &strings {
                // SAFETY: `host_ptr`, `StrView`, and `out` are valid for this call.
                let rc = unsafe { intern_fn(host_ptr, StrView::from_str(s), &raw mut out) };
                debug_assert_eq!(rc, FfiResult::OK);
            }
            let elapsed = start.elapsed();

            black_box(out);
            assert_eq!(world.resource::<BamSymbolTable>().len(), unique_count);
            elapsed
        },
        cfg.warmup,
        cfg.rounds,
        unique_count,
    );
    print_row(
        &format!("host_intern_symbol unique ({unique_count})"),
        unique_stats,
        "sym",
    );

    // 2b. `lookup_count` repeated deduplicated lookups against populated table
    {
        let mut world = World::new();
        world.insert_resource(WorldDropGuard::new());
        world.insert_resource(ModRegistry::new());
        let _ = ensure_symbol_tables_in_world(&mut world);
        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let host_ctx = HostContext::new(snap);
        let host_ptr = host_ctx.as_ffi_ptr();

        let mut out = BamSymbol::INVALID;
        for s in &strings {
            // SAFETY: Valid `host_ptr`, `StrView`, and `out`.
            let rc = unsafe { intern_fn(host_ptr, StrView::from_str(s), &raw mut out) };
            assert_eq!(rc, FfiResult::OK);
        }

        let lookup_stats = measure_fn(
            || {
                let mut acc = 0u64;
                let start = Instant::now();
                for i in 0..lookup_count {
                    let s = &strings[i % unique_count];
                    // SAFETY: Valid `host_ptr`, `StrView`, and `out`.
                    let rc = unsafe { intern_fn(host_ptr, StrView::from_str(s), &raw mut out) };
                    debug_assert_eq!(rc, FfiResult::OK);
                    acc = acc.wrapping_add(u64::from(out.to_raw()));
                }
                let elapsed = start.elapsed();
                black_box(acc);
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            lookup_count,
        );
        print_row(
            &format!("host_intern_symbol lookup ({lookup_count})"),
            lookup_stats,
            "sym",
        );
    }
}

// ============================================================================
// Scenario 3: `iter_count` vs full chunk iteration count at `10k` / `100k`
// ============================================================================

fn spawn_16_table_positions(world: &mut World, count: usize) -> Vec<Entity> {
    let mut entities = Vec::with_capacity(count);
    for i in 0..count {
        let tid = i & 15;
        let mut e = world.spawn(Position {
            x: i as f32,
            y: 1.0,
            z: 2.0,
        });
        if (tid & 1) != 0 {
            e.insert(TableBit0);
        }
        if (tid & 2) != 0 {
            e.insert(TableBit1);
        }
        if (tid & 4) != 0 {
            e.insert(TableBit2);
        }
        if (tid & 8) != 0 {
            e.insert(TableBit3);
        }
        entities.push(e.id());
    }
    entities
}

fn bench_iter_count(cfg: &BenchConfig) {
    let scales: &[usize] = if cfg.test_mode {
        &[128]
    } else {
        &[10_000, 100_000]
    };
    println!("\n================================================================================");
    println!(" SCENARIO 3: host_iter_count vs Full Chunk Iteration Count (10k / 100k)");
    println!("================================================================================");

    let iter_count_fn = HOST_API
        .iter_count
        .expect("HOST_API.iter_count must be wired");

    for &count in scales {
        println!(
            "\n--- iter_count/{count} ({} rounds, 16 tables) ---",
            cfg.rounds
        );

        let mut world = World::new();
        let pos_cid = world.register_component::<Position>();
        let entities = spawn_16_table_positions(&mut world, count);

        // Advance tick and dirty every even entity (50% dirty runs across 16 tables).
        let t_after_spawn = world.increment_change_tick();
        world.increment_change_tick();
        for &e in entities.iter().step_by(2) {
            if let Some(mut p) = world.get_mut::<Position>(e) {
                p.x += 1.0;
            }
        }
        let t_now = world.read_change_tick();
        let expected_changed = count.div_ceil(2);

        let matched_tables: Vec<TableId> = {
            let mut tables = Vec::new();
            for arch in world.archetypes().iter() {
                if arch.contains(pos_cid) && !arch.is_empty() {
                    let tid = arch.table_id();
                    if !tables.contains(&tid) {
                        tables.push(tid);
                    }
                }
            }
            tables
        };

        let make_plan = |mode: AccessMode| {
            QueryPlan::from_terms(vec![QueryTermMeta {
                component: BamComponent::from_raw(1),
                component_id: pos_cid,
                mode,
                flags: 0,
                uuid: Position::UUID,
                size: core::mem::size_of::<Position>(),
                align: core::mem::align_of::<Position>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            }])
        };

        let unfilt_plan = make_plan(AccessMode::READ);
        let changed_plan = make_plan(AccessMode::CHANGED);

        // 3a. Unfiltered `host_iter_count` vs `host_iter_next_chunk` loop
        let mut iter_unfilt = HostIter::new_dense(
            &world.storages().tables,
            &matched_tables,
            &unfilt_plan,
            Tick::new(0),
            t_now,
        );
        let unfilt_count_stats = measure_fn(
            || {
                let mut n = 0usize;
                let start = Instant::now();
                // SAFETY: `iter_unfilt` and `n` are valid aligned stack values.
                let rc = unsafe { iter_count_fn(iter_unfilt.as_ffi_ptr(), &raw mut n) };
                let elapsed = start.elapsed();
                debug_assert_eq!(rc, FfiResult::OK);
                assert_eq!(n, count);
                black_box(n);
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            count,
        );
        print_row(
            &format!("host_iter_count unfiltered ({count})"),
            unfilt_count_stats,
            "ent",
        );

        let unfilt_chunk_stats = measure_fn(
            || {
                // SAFETY: `iter_unfilt` is valid on stack.
                let _ = unsafe { host_iter_reset(iter_unfilt.as_ffi_ptr()) };
                let mut total = 0usize;
                let mut len = 0usize;
                let start = Instant::now();
                // SAFETY: Advancing `iter_unfilt` across matched dense chunks.
                while unsafe { host_iter_next_chunk(iter_unfilt.as_ffi_ptr(), &raw mut len) }.get()
                {
                    total += len;
                }
                let elapsed = start.elapsed();
                assert_eq!(total, count);
                black_box(total);
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            count,
        );
        print_row(
            &format!("Chunk iter count unfiltered ({count})"),
            unfilt_chunk_stats,
            "ent",
        );

        // 3b. `Changed<Position>` (50% dirty) `host_iter_count` vs `host_iter_next_chunk` loop
        let mut iter_changed = HostIter::new_dense(
            &world.storages().tables,
            &matched_tables,
            &changed_plan,
            t_after_spawn,
            t_now,
        );
        let changed_count_stats = measure_fn(
            || {
                let mut n = 0usize;
                let start = Instant::now();
                // SAFETY: `iter_changed` and `n` are valid aligned stack values.
                let rc = unsafe { iter_count_fn(iter_changed.as_ffi_ptr(), &raw mut n) };
                let elapsed = start.elapsed();
                debug_assert_eq!(rc, FfiResult::OK);
                assert_eq!(n, expected_changed);
                black_box(n);
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            count,
        );
        print_row(
            &format!("host_iter_count Changed 50% ({count})"),
            changed_count_stats,
            "ent",
        );

        let changed_chunk_stats = measure_fn(
            || {
                // SAFETY: `iter_changed` is valid on stack.
                let _ = unsafe { host_iter_reset(iter_changed.as_ffi_ptr()) };
                let mut total = 0usize;
                let mut len = 0usize;
                let start = Instant::now();
                // SAFETY: Advancing `iter_changed` across dirty contiguous runs.
                while unsafe { host_iter_next_chunk(iter_changed.as_ffi_ptr(), &raw mut len) }.get()
                {
                    total += len;
                }
                let elapsed = start.elapsed();
                assert_eq!(total, expected_changed);
                black_box(total);
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            count,
        );
        print_row(
            &format!("Chunk iter count Changed 50% ({count})"),
            changed_chunk_stats,
            "ent",
        );
    }
}

// ============================================================================
// Scenario 4: `checkpoint_rollback` (`10k` commands, `cmd_checkpoint` + `cmd_rollback`)
// ============================================================================

fn bench_checkpoint_rollback(cfg: &BenchConfig) {
    let count = cfg.scale_count(10_000);
    println!("\n================================================================================");
    println!(" SCENARIO 4: Command Checkpoint & Rollback ({count} commands)");
    println!("================================================================================");

    let checkpoint_fn = HOST_API
        .cmd_checkpoint
        .expect("HOST_API.cmd_checkpoint must be wired");
    let rollback_fn = HOST_API
        .cmd_rollback
        .expect("HOST_API.cmd_rollback must be wired");
    let insert_fn = HOST_API
        .cmd_insert
        .expect("HOST_API.cmd_insert must be wired");

    let mut world = World::new();
    let pos_cid = world.register_component::<Position>();
    let mut reg = ModRegistry::new();
    let pos_h = reg
        .register_component::<Position>(pos_cid, ComponentOptions::new().with_raw_insert())
        .expect("register Position");
    let snap = reg.snapshot_arc();
    let target = world.spawn_empty().id();
    let target_ffi = ffi_ent(target);
    let payload = Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let mut storage = HostCommandsStorage::new();

    // 4a. Full round-trip: `cmd_checkpoint` + `count` `cmd_insert` + `cmd_rollback`
    let roundtrip_stats = measure_fn(
        || {
            let start = Instant::now();
            {
                let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                let cmds_ptr = cmds.as_ffi_ptr();
                let mut cp = 0usize;
                // SAFETY: `cmds_ptr` is active and `cp` is valid stack storage.
                let rc_cp = unsafe { checkpoint_fn(cmds_ptr, &raw mut cp) };
                debug_assert_eq!(rc_cp, FfiResult::OK);
                for _ in 0..count {
                    // SAFETY: `cmds_ptr` is active, `target_ffi` is valid, and `payload` is
                    // readable for `size_of::<Position>()` bytes.
                    let rc = unsafe {
                        insert_fn(
                            cmds_ptr,
                            target_ffi,
                            pos_h,
                            (&raw const payload).cast::<u8>(),
                            core::mem::size_of::<Position>(),
                        )
                    };
                    debug_assert_eq!(rc, FfiResult::OK);
                }
                // SAFETY: `cmds_ptr` is active and `cp` is a valid checkpoint token.
                let rc_rb = unsafe { rollback_fn(cmds_ptr, cp) };
                debug_assert_eq!(rc_rb, FfiResult::OK);
            }
            let elapsed = start.elapsed();
            storage.clear();
            elapsed
        },
        cfg.warmup,
        cfg.rounds,
        count,
    );
    print_row(
        &format!("checkpoint + {count} cmd_insert + rollback"),
        roundtrip_stats,
        "cmd",
    );

    // 4b. Isolated `cmd_rollback` over `count` already-queued commands
    let rollback_only_stats = measure_fn(
        || {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let cmds_ptr = cmds.as_ffi_ptr();
            let mut cp = 0usize;
            // SAFETY: Valid `cmds_ptr` and stack `cp`.
            let rc_cp = unsafe { checkpoint_fn(cmds_ptr, &raw mut cp) };
            debug_assert_eq!(rc_cp, FfiResult::OK);
            for _ in 0..count {
                // SAFETY: Valid `cmds_ptr`, live `target_ffi`, and POD `payload`.
                let rc = unsafe {
                    insert_fn(
                        cmds_ptr,
                        target_ffi,
                        pos_h,
                        (&raw const payload).cast::<u8>(),
                        core::mem::size_of::<Position>(),
                    )
                };
                debug_assert_eq!(rc, FfiResult::OK);
            }

            let start = Instant::now();
            // SAFETY: Valid `cmds_ptr` and checkpoint `cp`.
            let rc_rb = unsafe { rollback_fn(cmds_ptr, cp) };
            let elapsed = start.elapsed();
            debug_assert_eq!(rc_rb, FfiResult::OK);

            drop(cmds);
            storage.clear();
            elapsed
        },
        cfg.warmup,
        cfg.rounds,
        count,
    );
    assert!(world.entity(target).get::<Position>().is_none());
    print_row(
        &format!("cmd_rollback only ({count} cmds)"),
        rollback_only_stats,
        "cmd",
    );
}

// ============================================================================
// Scenario 5: `migrate_column` (`migrate_component_column` over `10k` entities)
// ============================================================================

unsafe extern "C" fn widen_coord_cb(
    src_row: *const c_void,
    dst_row: *mut c_void,
    _user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `src_row` points to initialized `CoordV1` storage.
    let old = unsafe { core::ptr::read(src_row.cast::<CoordV1>()) };
    // SAFETY: `dst_row` points to aligned writable `CoordV2` storage.
    unsafe {
        core::ptr::write(
            dst_row.cast::<CoordV2>(),
            CoordV2 {
                x: f64::from(old.x) * 2.0,
                y: f64::from(old.y) + 100.0,
            },
        );
    }
    FfiResult::OK
}

fn bench_migrate_column(cfg: &BenchConfig) {
    let count = cfg.scale_count(10_000);
    println!("\n================================================================================");
    println!(" SCENARIO 5: Live Column Schema Migration (8B -> 16B over {count} entities)");
    println!("================================================================================");

    const UUID_V1: ComponentUuid = ComponentUuid::from_parts("v03_bench::Coord", 8, 4, 1);
    const UUID_V2: ComponentUuid = ComponentUuid::from_parts("v03_bench::Coord", 16, 8, 2);
    const FP_V1: u64 = 0xAAAA_BBBB_CCCC_0001;
    const FP_V2: u64 = 0xAAAA_BBBB_CCCC_0002;
    const FIELDS_V2: [BamFieldDesc; 2] = [
        BamFieldDesc::new(
            BamFieldKind::F64,
            StrView::from_str("x"),
            StrView::from_str("f64"),
            0,
            8,
            8,
        ),
        BamFieldDesc::new(
            BamFieldKind::F64,
            StrView::from_str("y"),
            StrView::from_str("f64"),
            8,
            8,
            8,
        ),
    ];

    let migrate_slot = HOST_API
        .migrate_component_column
        .expect("HOST_API.migrate_component_column must be wired");

    let stats = measure_fn(
        || {
            let mut world = World::new();
            world.insert_resource(WorldDropGuard::new());
            world.insert_resource(ModRegistry::new());
            let active = Arc::new(AtomicBool::new(true));

            let mut handle = BamComponent::INVALID;
            {
                let reg = world.resource::<ModRegistry>().clone();
                let mut load_ctx = LoadContext::with_world_and_mod_id(
                    reg,
                    Arc::clone(&active),
                    &raw mut world,
                    None,
                );
                let desc_v1 = ComponentDesc::new(
                    c"v03_bench::Coord".as_ptr(),
                    UUID_V1,
                    core::mem::size_of::<CoordV1>(),
                    core::mem::align_of::<CoordV1>(),
                )
                .with_schema_fingerprint(FP_V1);
                // SAFETY: Valid `load_ctx`, `desc_v1`, and `handle` pointers.
                let rc = unsafe {
                    host_register_component(
                        load_ctx.as_ffi_ptr(),
                        &raw const desc_v1,
                        &raw mut handle,
                    )
                };
                assert_eq!(rc, FfiResult::OK);
                world.insert_resource(load_ctx.registry.clone());
                load_ctx.finish_init();
            }

            let old_comp_id = world
                .resource::<ModRegistry>()
                .get_by_handle(handle)
                .expect("registered CoordV1")
                .component_id;
            let mut entities = Vec::with_capacity(count);
            for i in 0..count {
                let mut e = world.spawn_empty();
                let val = CoordV1 {
                    x: i as f32,
                    y: -(i as f32),
                };
                OwningPtr::make(val, |ptr| {
                    // SAFETY: `old_comp_id` matches `CoordV1` layout.
                    unsafe {
                        e.insert_by_id(old_comp_id, ptr);
                    }
                });
                entities.push(e.id());
            }

            let new_desc = ComponentDesc::new(
                c"v03_bench::Coord".as_ptr(),
                UUID_V2,
                core::mem::size_of::<CoordV2>(),
                core::mem::align_of::<CoordV2>(),
            )
            .with_schema_fingerprint(FP_V2)
            .with_fields(FIELDS_V2.as_ptr(), FIELDS_V2.len());

            let reg = world.resource::<ModRegistry>().clone();
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, active, &raw mut world, None);
            let lp = load_ctx.as_ffi_ptr();

            let start = Instant::now();
            // SAFETY: Valid `lp`, registered `handle`, `new_desc`, and `widen_coord_cb`.
            let rc = unsafe {
                migrate_slot(
                    lp,
                    handle,
                    FP_V1,
                    &raw const new_desc,
                    Some(widen_coord_cb),
                    null_mut(),
                )
            };
            let elapsed = start.elapsed();

            assert_eq!(rc, FfiResult::OK);
            load_ctx.finish_init();
            let new_entry = world
                .resource::<ModRegistry>()
                .get_by_handle(handle)
                .expect("migrated entry");
            assert_eq!(new_entry.size, 16);
            if let Some(&last_e) = entities.last() {
                // SAFETY: `new_entry.component_id` holds initialized `CoordV2`.
                let last_val = unsafe {
                    *world
                        .entity(last_e)
                        .get_by_id(new_entry.component_id)
                        .expect("migrated component")
                        .deref::<CoordV2>()
                };
                let idx = (count - 1) as f64;
                assert_eq!(
                    last_val,
                    CoordV2 {
                        x: idx * 2.0,
                        y: -idx + 100.0,
                    }
                );
            }
            elapsed
        },
        cfg.warmup,
        cfg.rounds,
        count,
    );
    print_row(
        &format!("migrate_component_column 8B->16B ({count})"),
        stats,
        "ent",
    );
}

// ============================================================================
// Scenario 6: `par_readonly` (`par_for_each_chunk_readonly` vs sequential at `100k`)
// ============================================================================

struct ParReadonlyBenchAcc {
    entities: AtomicUsize,
    xor_hash: AtomicU64,
    sum_hash: AtomicU64,
}

#[inline]
fn fold_readonly_row(
    ent_bits: u64,
    off: LocalOffset,
    opt_bonus: i64,
    has_tag: bool,
    scale: i64,
) -> u64 {
    let mut h = ent_bits.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= (off.dx as u64).rotate_left(7);
    h = h.wrapping_add((off.dy as u64).wrapping_mul(31));
    h ^= (off.dz as u64).rotate_left(19);
    h = h.wrapping_add((opt_bonus as u64).wrapping_mul(131));
    h ^= u64::from(has_tag).rotate_left(3);
    h.wrapping_mul((scale as u64) | 1)
}

#[expect(
    clippy::cast_ptr_alignment,
    reason = "Read-only host column and resource pointers are aligned to their declared types."
)]
unsafe fn reduce_readonly_iter(iter: *mut BamIter) -> (usize, u64, u64) {
    // SAFETY: Term 3 is `RESOURCE_READ` `ScaleRes`.
    let res_ptr = unsafe { host_res_ptr(iter.cast_const(), 3) };
    let scale = if res_ptr.is_null() {
        1
    } else {
        // SAFETY: `res_ptr` points to live aligned `ScaleRes`.
        unsafe { (*res_ptr.cast::<ScaleRes>()).factor }
    };

    let mut local_entities = 0usize;
    let mut local_xor = 0u64;
    let mut local_sum = 0u64;
    let mut len = 0usize;

    // SAFETY: `iter` is active and valid for chunk advancement.
    while unsafe { host_iter_next_chunk(iter, &raw mut len) }.get() {
        // SAFETY: Active chunk has `len` entities.
        let ents_ptr = unsafe { host_iter_entities(iter.cast_const()) };
        // SAFETY: Term 0 is dense `LocalOffset`.
        let off_ptr = unsafe { host_iter_column_readonly(iter.cast_const(), 0) };
        // SAFETY: Term 1 is `OPTIONAL_READ` `OptBonus`.
        let opt_ptr = unsafe { host_iter_column_readonly(iter.cast_const(), 1) };
        // SAFETY: Term 2 is `HAS` `FilterTagA`.
        let has_ptr = unsafe { host_iter_column_readonly(iter.cast_const(), 2) };
        if ents_ptr.is_null() || off_ptr.is_null() || has_ptr.is_null() {
            break;
        }
        // SAFETY: All slices have exact length `len` matching the current chunk.
        let ents = unsafe { core::slice::from_raw_parts(ents_ptr, len) };
        // SAFETY: `off_ptr` points to `len` contiguous `LocalOffset` elements.
        let offs = unsafe { core::slice::from_raw_parts(off_ptr.cast::<LocalOffset>(), len) };
        // SAFETY: `has_ptr` points to `len` contiguous `FfiBool` elements.
        let has_slice = unsafe { core::slice::from_raw_parts(has_ptr.cast::<FfiBool>(), len) };
        let opt_slice = if opt_ptr.is_null() {
            None
        } else {
            // SAFETY: Non-null `opt_ptr` points to `len` contiguous `OptBonus` elements.
            Some(unsafe { core::slice::from_raw_parts(opt_ptr.cast::<OptBonus>(), len) })
        };

        for i in 0..len {
            let bonus = opt_slice.map_or(0, |s| s[i].bonus);
            let h = fold_readonly_row(ents[i].to_bits(), offs[i], bonus, has_slice[i].get(), scale);
            local_xor ^= h;
            local_sum = local_sum.wrapping_add(h);
        }
        local_entities += len;
    }

    (local_entities, local_xor, local_sum)
}

unsafe extern "C" fn c_abi_seq_readonly_sys(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `ParReadonlyBenchAcc` outliving the `App`.
    let acc = unsafe { &*user_data.cast::<ParReadonlyBenchAcc>() };
    let mut iter: *mut BamIter = null_mut();
    // SAFETY: Query 0 is declared on `sys_ctx`.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &raw mut iter) };
    if rc.is_err() {
        return rc;
    }
    // SAFETY: `iter` is a valid active read-only iterator.
    let (entities, xor_hash, sum_hash) = unsafe { reduce_readonly_iter(iter) };
    acc.entities.store(entities, Ordering::Relaxed);
    acc.xor_hash.store(xor_hash, Ordering::Relaxed);
    acc.sum_hash.store(sum_hash, Ordering::Relaxed);
    FfiResult::OK
}

unsafe extern "C" fn c_abi_par_readonly_worker(
    sub_iter: *mut BamIter,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `ParReadonlyBenchAcc` live across synchronous dispatch.
    let acc = unsafe { &*user_data.cast::<ParReadonlyBenchAcc>() };
    // SAFETY: `sub_iter` is a disjoint read-only worker `HostIter`.
    let (entities, xor_hash, sum_hash) = unsafe { reduce_readonly_iter(sub_iter) };
    acc.entities.fetch_add(entities, Ordering::Relaxed);
    acc.xor_hash.fetch_xor(xor_hash, Ordering::Relaxed);
    acc.sum_hash.fetch_add(sum_hash, Ordering::Relaxed);
    FfiResult::OK
}

unsafe extern "C" fn c_abi_par_readonly_sys(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `ParReadonlyBenchAcc` outliving the `App`.
    let acc = unsafe { &*user_data.cast::<ParReadonlyBenchAcc>() };
    acc.entities.store(0, Ordering::Relaxed);
    acc.xor_hash.store(0, Ordering::Relaxed);
    acc.sum_hash.store(0, Ordering::Relaxed);

    let mut iter: *mut BamIter = null_mut();
    // SAFETY: Query 0 is declared on `sys_ctx`.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &raw mut iter) };
    if rc.is_err() {
        return rc;
    }
    let par_ro_fn = HOST_API
        .par_for_each_chunk_readonly
        .expect("HOST_API.par_for_each_chunk_readonly must be wired");
    // SAFETY: Query 0 contains only read-only terms and `acc` is `Sync`.
    unsafe { par_ro_fn(iter, Some(c_abi_par_readonly_worker), user_data) }
}

fn spawn_par_readonly_entities(world: &mut World, count: usize) {
    for i in 0..count as i64 {
        let off = LocalOffset {
            dx: i,
            dy: -i * 3,
            dz: (i % 97) * 5,
        };
        match i % 4 {
            0 => {
                world.spawn(off);
            }
            1 => {
                world.spawn((off, OptBonus { bonus: i * 11 }));
            }
            2 => {
                world.spawn((off, FilterTagA));
            }
            _ => {
                world.spawn((off, OptBonus { bonus: i * 13 }, FilterTagA));
            }
        }
    }
}

fn bench_par_readonly(cfg: &BenchConfig) {
    let count = cfg.scale_count(100_000);
    println!("\n================================================================================");
    println!(" SCENARIO 6: Read-Only Parallel Chunk Dispatch vs Sequential ({count} entities)");
    println!("================================================================================");

    let setup_world = |sys_cb| -> (App, Box<ParReadonlyBenchAcc>) {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.world_mut().insert_resource(ScaleRes { factor: 17 });
        let off_h = app
            .register_mod_component::<LocalOffset>(ComponentOptions::new())
            .expect("register LocalOffset");
        let opt_h = app
            .register_mod_component::<OptBonus>(ComponentOptions::new())
            .expect("register OptBonus");
        let tag_h = app
            .register_mod_component::<FilterTagA>(ComponentOptions::new())
            .expect("register FilterTagA");
        let scale_h = app
            .register_mod_resource::<ScaleRes>()
            .expect("register ScaleRes");
        spawn_par_readonly_entities(app.world_mut(), count);

        let reg = app.world().resource::<ModRegistry>().clone();
        let spec = BamQuerySpecOwned::new(vec![
            BamTermDesc::read(off_h),
            BamTermDesc::optional_read(opt_h),
            BamTermDesc::new(tag_h, AccessMode::HAS),
            BamTermDesc::resource_read(scale_h),
        ]);
        let acc = Box::new(ParReadonlyBenchAcc {
            entities: AtomicUsize::new(0),
            xor_hash: AtomicU64::new(0),
            sum_hash: AtomicU64::new(0),
        });
        let ud = (&*acc as *const ParReadonlyBenchAcc)
            .cast_mut()
            .cast::<c_void>();
        app.add_systems(
            Update,
            GuestSystem::new("bench_par_ro", vec![spec], sys_cb, ud, reg),
        );
        (app, acc)
    };

    let (mut seq_app, seq_acc) = setup_world(c_abi_seq_readonly_sys);
    let seq_stats = measure_fn(|| timed_update(&mut seq_app), cfg.warmup, cfg.rounds, count);
    let seq_entities = seq_acc.entities.load(Ordering::Relaxed);
    let seq_xor = seq_acc.xor_hash.load(Ordering::Relaxed);
    let seq_sum = seq_acc.sum_hash.load(Ordering::Relaxed);
    assert_eq!(seq_entities, count);
    print_row(
        &format!("Sequential chunk iteration ({count})"),
        seq_stats,
        "ent",
    );

    let (mut par_app, par_acc) = setup_world(c_abi_par_readonly_sys);
    let par_stats = measure_fn(|| timed_update(&mut par_app), cfg.warmup, cfg.rounds, count);
    let par_entities = par_acc.entities.load(Ordering::Relaxed);
    let par_xor = par_acc.xor_hash.load(Ordering::Relaxed);
    let par_sum = par_acc.sum_hash.load(Ordering::Relaxed);
    assert_eq!(par_entities, seq_entities);
    assert_eq!(par_xor, seq_xor);
    assert_eq!(par_sum, seq_sum);
    print_row(
        &format!("par_for_each_chunk_readonly ({count})"),
        par_stats,
        "ent",
    );

    let speedup = seq_stats.median_ms / par_stats.median_ms.max(1e-9);
    println!("  -> par_for_each_chunk_readonly speedup over sequential: {speedup:.2}x");
}

// ============================================================================
// Scenario 7: `up_cascade` (`iter_up_cascade` on depth-`64` hierarchy x `1k` leaves)
// ============================================================================

struct UpCascadeBenchCtx {
    leaves_visited: AtomicUsize,
    rgba_sum: AtomicU64,
}

unsafe fn walk_up_cascade_iter(iter: *mut BamIter) -> Result<(usize, u64), FfiResult> {
    let up_cascade_fn = HOST_API
        .iter_up_cascade
        .expect("HOST_API.iter_up_cascade must be wired");
    let mut count = 0usize;
    let mut sum = 0u64;
    let mut len = 0usize;

    // SAFETY: `iter` is active and valid for chunk iteration.
    while unsafe { host_iter_next_chunk(iter, &raw mut len) }.get() {
        // SAFETY: Active chunk has `len` leaf entities.
        let ents_ptr = unsafe { host_iter_entities(iter.cast_const()) };
        if ents_ptr.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        // SAFETY: `ents_ptr` covers `len` `FfiEntity` values.
        let ents = unsafe { core::slice::from_raw_parts(ents_ptr, len) };
        for &ent in ents {
            let mut ptr: *const c_void = null();
            let mut owner = FfiEntity::NULL;
            // SAFETY: `iter` has `world` attached and term 1 is `OPTIONAL_READ` `ThemeColor`.
            let rc = unsafe {
                up_cascade_fn(
                    iter,
                    BamComponent::INVALID,
                    ent,
                    1,
                    &raw mut ptr,
                    &raw mut owner,
                )
            };
            if rc.is_err() {
                return Err(rc);
            }
            // SAFETY: `ptr` points to a live `ThemeColor` on the resolved ancestor.
            let rgba = unsafe { (*ptr.cast::<ThemeColor>()).rgba };
            sum = sum.wrapping_add(u64::from(rgba));
            count += 1;
        }
    }
    Ok((count, sum))
}

unsafe extern "C" fn c_abi_up_cascade_seq_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `UpCascadeBenchCtx` outliving the `App`.
    let state = unsafe { &*user_data.cast::<UpCascadeBenchCtx>() };
    let mut iter: *mut BamIter = null_mut();
    // SAFETY: Query 0 is declared on `sys_ctx`.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &raw mut iter) };
    if rc.is_err() {
        return rc;
    }
    // SAFETY: `iter` is active for this system invocation.
    match unsafe { walk_up_cascade_iter(iter) } {
        Ok((cnt, sum)) => {
            state.leaves_visited.store(cnt, Ordering::Relaxed);
            state.rgba_sum.store(sum, Ordering::Relaxed);
            FfiResult::OK
        }
        Err(err) => err,
    }
}

unsafe extern "C" fn c_abi_up_cascade_worker_cb(
    sub_iter: *mut BamIter,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `UpCascadeBenchCtx` live across synchronous dispatch.
    let state = unsafe { &*user_data.cast::<UpCascadeBenchCtx>() };
    // SAFETY: `sub_iter` is an active read-only worker `HostIter`.
    match unsafe { walk_up_cascade_iter(sub_iter) } {
        Ok((cnt, sum)) => {
            state.leaves_visited.fetch_add(cnt, Ordering::Relaxed);
            state.rgba_sum.fetch_add(sum, Ordering::Relaxed);
            FfiResult::OK
        }
        Err(err) => err,
    }
}

unsafe extern "C" fn c_abi_up_cascade_par_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `UpCascadeBenchCtx` outliving the `App`.
    let state = unsafe { &*user_data.cast::<UpCascadeBenchCtx>() };
    state.leaves_visited.store(0, Ordering::Relaxed);
    state.rgba_sum.store(0, Ordering::Relaxed);

    let mut iter: *mut BamIter = null_mut();
    // SAFETY: Query 0 is declared on `sys_ctx`.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &raw mut iter) };
    if rc.is_err() {
        return rc;
    }
    let par_ro_fn = HOST_API
        .par_for_each_chunk_readonly
        .expect("HOST_API.par_for_each_chunk_readonly must be wired");
    // SAFETY: Query 0 is read-only and `state` is `Sync`.
    unsafe { par_ro_fn(iter, Some(c_abi_up_cascade_worker_cb), user_data) }
}

fn spawn_depth_hierarchy_with_leaves(world: &mut World, depth: usize, leaves: usize) -> u64 {
    let mut expected_sum = 0u64;
    for leaf_idx in 0..leaves {
        let rgba = 0xDEAD_0000u32 + (leaf_idx as u32);
        expected_sum = expected_sum.wrapping_add(u64::from(rgba));
        let mut curr = world.spawn(ThemeColor { rgba }).id();
        for _ in 1..depth {
            curr = world.spawn(ChildOf(curr)).id();
        }
        world.spawn((
            LeafMarker {
                idx: leaf_idx as u32,
            },
            ChildOf(curr),
        ));
    }
    expected_sum
}

fn bench_up_cascade(cfg: &BenchConfig) {
    let depth = if cfg.test_mode { 8 } else { 64 };
    let leaves = if cfg.test_mode { 16 } else { 1_000 };
    let total_hops = depth * leaves;
    println!("\n================================================================================");
    println!(
        " SCENARIO 7: iter_up_cascade (depth-{depth} hierarchy x {leaves} leaves = {total_hops} hops)"
    );
    println!("================================================================================");

    let setup_app = |cb| -> (App, Box<UpCascadeBenchCtx>, u64) {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let leaf_h = app
            .register_mod_component::<LeafMarker>(ComponentOptions::new())
            .expect("register LeafMarker");
        let theme_h = app
            .register_mod_component::<ThemeColor>(ComponentOptions::new())
            .expect("register ThemeColor");
        let expected_sum = spawn_depth_hierarchy_with_leaves(app.world_mut(), depth, leaves);

        let reg = app.world().resource::<ModRegistry>().clone();
        let spec = BamQuerySpecOwned::new(vec![
            BamTermDesc::read(leaf_h),
            BamTermDesc::optional_read(theme_h),
        ]);
        let state = Box::new(UpCascadeBenchCtx {
            leaves_visited: AtomicUsize::new(0),
            rgba_sum: AtomicU64::new(0),
        });
        let ud = (&*state as *const UpCascadeBenchCtx)
            .cast_mut()
            .cast::<c_void>();
        app.add_systems(
            Update,
            GuestSystem::new("bench_up_cascade", vec![spec], cb, ud, reg),
        );
        (app, state, expected_sum)
    };

    let (mut seq_app, seq_state, expected_sum) = setup_app(c_abi_up_cascade_seq_cb);
    let seq_stats = measure_fn(
        || timed_update(&mut seq_app),
        cfg.warmup,
        cfg.rounds,
        leaves,
    );
    assert_eq!(seq_state.leaves_visited.load(Ordering::Relaxed), leaves);
    assert_eq!(seq_state.rgba_sum.load(Ordering::Relaxed), expected_sum);
    print_row(
        &format!("iter_up_cascade seq ({leaves}x{depth})"),
        seq_stats,
        "leaf",
    );

    let (mut par_app, par_state, expected_sum_par) = setup_app(c_abi_up_cascade_par_cb);
    let par_stats = measure_fn(
        || timed_update(&mut par_app),
        cfg.warmup,
        cfg.rounds,
        leaves,
    );
    assert_eq!(par_state.leaves_visited.load(Ordering::Relaxed), leaves);
    assert_eq!(par_state.rgba_sum.load(Ordering::Relaxed), expected_sum_par);
    print_row(
        &format!("iter_up_cascade par_ro ({leaves}x{depth})"),
        par_stats,
        "leaf",
    );
}

// ============================================================================
// Scenario 8: `schedule_set_rewire` (`configure_schedule_set` + `mount_system_in_schedule`)
// ============================================================================

unsafe extern "C" fn noop_sched_cb(
    _sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

fn configure_and_mount_schedule_sets(
    world: &mut World,
    set_count: usize,
    sys_per_set: usize,
) -> Vec<BamSystemSet> {
    let config_fn = HOST_API
        .configure_schedule_set
        .expect("HOST_API.configure_schedule_set must be wired");
    let reg_sys_fn = HOST_API
        .register_system_handle
        .expect("HOST_API.register_system_handle must be wired");
    let mount_fn = HOST_API
        .mount_system_in_schedule
        .expect("HOST_API.mount_system_in_schedule must be wired");

    let reg = world.resource::<ModRegistry>().clone();
    let mut load_ctx = LoadContext::with_manifest_name(
        reg,
        Arc::new(AtomicBool::new(true)),
        (&raw mut *world).cast::<World>(),
        Some(ModId(7)),
        Some(Arc::from("bench_sched")),
    );
    let lp = load_ctx.as_ffi_ptr();

    let mut set_handles = Vec::with_capacity(set_count);
    for s_idx in 0..set_count {
        let sname = CString::new(format!("Stage_{s_idx}")).expect("valid set name");
        let flags = if s_idx % 2 == 0 {
            BAM_SCHEDULE_SET_FLAG_SEQUENCE
        } else {
            BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK
        };
        let mut desc = BamScheduleSetDesc::new(sname.as_ptr()).with_flags(flags);
        let deps: [BamSystemSet; 2];
        if s_idx == 1 {
            deps = [set_handles[0], BamSystemSet::INVALID];
            desc.after_sets = deps.as_ptr();
            desc.after_count = 1;
        } else if s_idx == 2 {
            deps = [set_handles[0], set_handles[1]];
            desc.after_sets = deps.as_ptr();
            desc.after_count = 2;
        } else if s_idx > 2 {
            deps = [set_handles[s_idx - 1], BamSystemSet::INVALID];
            desc.after_sets = deps.as_ptr();
            desc.after_count = 1;
        }

        let mut set_h = BamSystemSet::INVALID;
        // SAFETY: `lp`, `desc`, and `set_h` are valid aligned pointers for this call.
        let rc = unsafe { config_fn(lp, &desc, &raw mut set_h) };
        assert_eq!(rc, FfiResult::OK);
        set_handles.push(set_h);
    }

    for (s_idx, &set_h) in set_handles.iter().enumerate() {
        for m_idx in 0..sys_per_set {
            let sys_name = CString::new(format!("sys_{s_idx}_{m_idx}")).expect("valid system name");
            let mut sys_h = BamSystem::INVALID;
            let sd = BamSystemDesc::new(sys_name.as_ptr(), null(), 0, noop_sched_cb, null_mut());
            // SAFETY: Valid `lp`, `sd`, and `sys_h`.
            let r_reg = unsafe { reg_sys_fn(lp, &sd, &raw mut sys_h) };
            assert_eq!(r_reg, FfiResult::OK);
            // SAFETY: Valid `lp`, `sys_h`, and `set_h`.
            let r_mnt = unsafe { mount_fn(lp, sys_h, ScheduleId::UPDATE, set_h) };
            assert_eq!(r_mnt, FfiResult::OK);
        }
    }

    let systems = core::mem::take(&mut load_ctx.registered_systems);
    load_ctx.finish_init();
    mount_mod_systems_in_world(world, "bench_sched", 0, systems)
        .expect("mount_mod_systems_in_world succeeds");
    set_handles
}

fn init_schedule_world() -> World {
    let mut world = World::new();
    world.insert_resource(ModHost::new());
    world.insert_resource(ModRegistry::new());
    world.insert_resource(ModScheduleMap::new());
    let mut schedules = Schedules::new();
    schedules.insert(Schedule::new(Update));
    world.insert_resource(schedules);
    world
}

fn bench_schedule_set_rewire(cfg: &BenchConfig) {
    let set_count = if cfg.test_mode { 2 } else { 8 };
    let sys_per_set = if cfg.test_mode { 2 } else { 4 };
    let total_systems = set_count * sys_per_set;
    println!("\n================================================================================");
    println!(
        " SCENARIO 8: Schedule Set Configure + Mount + Rewire ({set_count} sets, {total_systems} systems)"
    );
    println!("================================================================================");

    // 8a. Full configure_schedule_set + mount_system_in_schedule + initial schedule build
    let build_stats = measure_fn(
        || {
            let mut world = init_schedule_world();
            let start = Instant::now();
            let _sets = configure_and_mount_schedule_sets(&mut world, set_count, sys_per_set);
            start.elapsed()
        },
        cfg.warmup,
        cfg.rounds,
        total_systems,
    );
    print_row(
        &format!("configure+mount+build ({set_count}x{sys_per_set})"),
        build_stats,
        "sys",
    );

    // 8b. Steady-state hot-rewire rebuild cost (`rewire_mod_schedule_sets_in_world`)
    let mut world = init_schedule_world();
    let sets = configure_and_mount_schedule_sets(&mut world, set_count, sys_per_set);
    let mut flip = false;

    let rewire_stats = measure_fn(
        || {
            flip = !flip;
            {
                let mut set_reg = world.resource_mut::<ModScheduleSetRegistry>();
                if flip {
                    set_reg
                        .set_edges(sets[1], vec![sets[0]], vec![], false, false)
                        .expect("flip to before");
                } else {
                    set_reg
                        .set_edges(sets[1], vec![], vec![sets[0]], false, false)
                        .expect("flip to after");
                }
            }
            let start = Instant::now();
            rewire_mod_schedule_sets_in_world(&mut world, ScheduleId::UPDATE)
                .expect("rewire_mod_schedule_sets_in_world succeeds");
            start.elapsed()
        },
        cfg.warmup,
        cfg.rounds,
        total_systems,
    );
    world.run_schedule(Update);
    print_row(
        &format!("rewire_mod_schedule_sets ({total_systems} sys)"),
        rewire_stats,
        "sys",
    );
}

// ============================================================================
// Entrypoint
// ============================================================================

fn main() {
    let cfg = BenchConfig::from_args();

    println!("================================================================================");
    println!(
        " bamabi ABI 0.3.0 Subsystem Benchmark Suite [mode={}]",
        if cfg.test_mode {
            "smoke-test"
        } else if cfg.quick_mode {
            "quick"
        } else {
            "full"
        }
    );
    println!("================================================================================");

    if cfg.matches("relations") {
        bench_relations(&cfg);
    }
    if cfg.matches("intern_symbol") {
        bench_intern_symbol(&cfg);
    }
    if cfg.matches("iter_count") {
        bench_iter_count(&cfg);
    }
    if cfg.matches("checkpoint_rollback") {
        bench_checkpoint_rollback(&cfg);
    }
    if cfg.matches("migrate_column") {
        bench_migrate_column(&cfg);
    }
    if cfg.matches("par_readonly") {
        bench_par_readonly(&cfg);
    }
    if cfg.matches("up_cascade") {
        bench_up_cascade(&cfg);
    }
    if cfg.matches("schedule_set_rewire") {
        bench_schedule_set_rewire(&cfg);
    }

    println!("\nABI 0.3.0 subsystem benchmark suite completed cleanly.");
}
