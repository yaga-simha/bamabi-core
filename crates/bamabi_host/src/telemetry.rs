//! Opt-in headless `SystemTelemetryDigest64` (`"BAMT"`) telemetry sink and native Bevy
//! system execution wrapper with zero `bevy_reflect` dependency.

use crate::budgets::{ModBudgetTelemetry, ensure_budget_resources, flush_mod_budgets};
use bamabi_sys::{
    SYSTEM_TELEMETRY_DIGEST_VERSION, SYSTEM_TELEMETRY_MAGIC, ScheduleId, SystemTelemetryDigest64,
    fnv1a_64_hash_system,
};
use bevy_app::{App, Last, Plugin};
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_ecs::system::{IntoSystem, Res, ResMut, System};
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use bevy_platform::time::Instant;
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::Arc;

/// Default maximum [`SystemTelemetryDigest64`] records retained in [`ModTelemetryRing`].
pub const DEFAULT_TELEMETRY_RING_CAPACITY: usize = 1024;

/// Callback type receiving batches of freshly produced binary [`SystemTelemetryDigest64`] frames.
pub type TelemetryDigestSinkFn = Arc<dyn Fn(&[SystemTelemetryDigest64]) + Send + Sync>;

#[derive(Resource, Default, Debug, Clone, Copy)]
struct TelemetryFrameSeq(u64);

#[derive(Resource, Clone)]
struct ModTelemetrySinkCallback(TelemetryDigestSinkFn);

/// Bounded FIFO ring buffer retaining the most recent [`SystemTelemetryDigest64`] telemetry frames
/// emitted by guest systems (via [`ModBudgetTelemetry`]) and instrumented native Bevy systems.
#[derive(Resource, Clone, Debug)]
pub struct ModTelemetryRing {
    capacity: usize,
    entries: VecDeque<SystemTelemetryDigest64>,
}

impl Default for ModTelemetryRing {
    #[inline]
    fn default() -> Self {
        Self::with_capacity(DEFAULT_TELEMETRY_RING_CAPACITY)
    }
}

impl ModTelemetryRing {
    /// Creates an empty [`ModTelemetryRing`] retaining up to `capacity.max(1)` frames.
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            capacity: cap,
            entries: VecDeque::with_capacity(cap),
        }
    }

    /// Returns the configured ring capacity.
    #[inline]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of buffered [`SystemTelemetryDigest64`] frames.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no telemetry frames are currently buffered.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Pushes `digest` onto the ring, evicting the oldest frame if at capacity.
    pub fn push(&mut self, digest: SystemTelemetryDigest64) {
        while self.entries.len() >= self.capacity {
            let _ = self.entries.pop_front();
        }
        self.entries.push_back(digest);
    }

    /// Iterates over buffered telemetry frames from oldest to newest.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &SystemTelemetryDigest64> {
        self.entries.iter()
    }

    /// Returns a contiguous snapshot `Vec<SystemTelemetryDigest64>` of buffered frames.
    #[inline]
    pub fn snapshot(&self) -> Vec<SystemTelemetryDigest64> {
        self.entries.iter().copied().collect()
    }

    /// Drains all buffered frames in oldest-to-newest order.
    #[inline]
    pub fn drain(&mut self) -> Vec<SystemTelemetryDigest64> {
        self.entries.drain(..).collect()
    }

    /// Clears all buffered frames.
    #[inline]
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[derive(Default)]
struct NativeLatencyWindow {
    samples: Vec<u64>,
    cursor: usize,
}

impl NativeLatencyWindow {
    const WINDOW_SIZE: usize = 32;

    fn record_percentiles(&mut self, duration_us: u64) -> (u32, u32) {
        if self.samples.len() < Self::WINDOW_SIZE {
            self.samples.push(duration_us);
        } else {
            self.samples[self.cursor] = duration_us;
            self.cursor = (self.cursor + 1) % Self::WINDOW_SIZE;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        let p50_idx = (n.saturating_mul(50).div_ceil(100))
            .saturating_sub(1)
            .min(n - 1);
        let p99_idx = (n.saturating_mul(99).div_ceil(100))
            .saturating_sub(1)
            .min(n - 1);
        let p50 = u32::try_from(sorted[p50_idx]).unwrap_or(u32::MAX);
        let p99 = u32::try_from(sorted[p99_idx]).unwrap_or(u32::MAX);
        (p50, p99)
    }
}

/// World resource tracking rolling `p50_us` / `p99_us` execution latencies for native Bevy systems
/// wrapped via [`wrap_native_system`] or [`with_native_system_telemetry`].
#[derive(Resource, Default)]
pub struct NativeSystemTelemetryTracker {
    windows: HashMap<(Arc<str>, Arc<str>), NativeLatencyWindow>,
}

impl NativeSystemTelemetryTracker {
    fn record(&mut self, scope: &Arc<str>, system_name: &Arc<str>, duration_us: u64) -> (u32, u32) {
        let win = self
            .windows
            .entry((Arc::clone(scope), Arc::clone(system_name)))
            .or_default();
        win.record_percentiles(duration_us)
    }
}

/// Records a single native Bevy system execution duration into [`ModTelemetryRing`] (and invokes
/// any configured sink callback), returning the encoded [`SystemTelemetryDigest64`].
pub fn record_native_system_run(
    world: &mut World,
    scope: &Arc<str>,
    system_name: &Arc<str>,
    schedule_id: ScheduleId,
    elapsed: Duration,
    flags: u16,
) -> SystemTelemetryDigest64 {
    let _ = schedule_id;
    world.init_resource::<ModTelemetryRing>();
    world.init_resource::<NativeSystemTelemetryTracker>();
    world.init_resource::<TelemetryFrameSeq>();

    let frame_seq = world.resource::<TelemetryFrameSeq>().0;
    let duration_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    let (p50_us, p99_us) = world.resource_mut::<NativeSystemTelemetryTracker>().record(
        scope,
        system_name,
        duration_us,
    );

    let digest = SystemTelemetryDigest64 {
        magic: SYSTEM_TELEMETRY_MAGIC,
        version: SYSTEM_TELEMETRY_DIGEST_VERSION,
        flags,
        mod_id: 0,
        system_hash: fnv1a_64_hash_system(scope.as_ref(), system_name.as_ref()),
        frame_seq,
        matched_entities: 0,
        matched_tables: 0,
        skipped_summary_tick_tables: 0,
        chunks: 0,
        p50_us,
        p99_us,
        commands_queued: 0,
        reserved: 0,
    };

    world.resource_mut::<ModTelemetryRing>().push(digest);
    if let Some(sink) = world.get_resource::<ModTelemetrySinkCallback>() {
        (sink.0)(core::slice::from_ref(&digest));
    }
    digest
}

/// Executes a closure `f(&mut World)` as an instrumented native Bevy step and records a
/// [`SystemTelemetryDigest64`] frame into `world`'s [`ModTelemetryRing`].
pub fn with_native_system_telemetry<F, R>(
    world: &mut World,
    scope: impl Into<Arc<str>>,
    system_name: impl Into<Arc<str>>,
    schedule_id: ScheduleId,
    f: F,
) -> R
where
    F: FnOnce(&mut World) -> R,
{
    let scope_arc: Arc<str> = scope.into();
    let name_arc: Arc<str> = system_name.into();
    let start = Instant::now();
    let result = f(world);
    let elapsed = start.elapsed();
    let _ = record_native_system_run(world, &scope_arc, &name_arc, schedule_id, elapsed, 0);
    result
}

/// Wraps any native Bevy system (`IntoSystem<(), (), Marker>`) into a reusable, reflection-free
/// exclusive system that measures execution time and records [`SystemTelemetryDigest64`] frames into
/// [`ModTelemetryRing`].
pub fn wrap_native_system<Marker, S>(
    scope: impl Into<Arc<str>>,
    system_name: impl Into<Arc<str>>,
    schedule_id: ScheduleId,
    system: S,
) -> impl FnMut(&mut World) + Send + Sync + 'static
where
    S: IntoSystem<(), (), Marker> + Send + Sync + 'static,
    S::System: Send + Sync + 'static,
{
    let scope_arc: Arc<str> = scope.into();
    let name_arc: Arc<str> = system_name.into();
    let mut inner = IntoSystem::into_system(system);
    let mut initialized = false;
    move |world: &mut World| {
        if !initialized {
            let _ = System::initialize(&mut inner, world);
            initialized = true;
        }
        let start = Instant::now();
        let _ = System::run(&mut inner, (), world);
        let elapsed = start.elapsed();
        let _ = record_native_system_run(world, &scope_arc, &name_arc, schedule_id, elapsed, 0);
    }
}

/// Exclusive `Last` system converting current [`ModBudgetTelemetry`] snapshots into `"BAMT"`
/// [`SystemTelemetryDigest64`] frames, appending them to [`ModTelemetryRing`], and forwarding them
/// to any registered [`TelemetryDigestSinkFn`].
fn flush_mod_telemetry_digests(
    telemetry: Res<ModBudgetTelemetry>,
    mut ring: ResMut<ModTelemetryRing>,
    mut frame_seq: ResMut<TelemetryFrameSeq>,
    sink: Option<Res<ModTelemetrySinkCallback>>,
) {
    let seq = frame_seq.0;
    frame_seq.0 = frame_seq.0.wrapping_add(1);

    let snapshots = telemetry.system_budget_snapshots();
    if snapshots.is_empty() {
        return;
    }

    let mut batch = Vec::with_capacity(snapshots.len());
    for snap in snapshots {
        let mut digest = snap.to_telemetry_digest64("", "", ScheduleId::UPDATE, 0);
        digest.frame_seq = seq;
        ring.push(digest);
        batch.push(digest);
    }
    if let Some(cb) = sink {
        (cb.0)(&batch);
    }
}

/// Opt-in headless Bevy [`Plugin`] collecting 64-byte [`SystemTelemetryDigest64`] telemetry frames
/// into [`ModTelemetryRing`] (and an optional streaming [`TelemetryDigestSinkFn`]) with zero
/// `bevy_reflect` dependency.
#[derive(Clone)]
pub struct ModTelemetrySinkPlugin {
    /// Maximum [`SystemTelemetryDigest64`] frames retained in [`ModTelemetryRing`].
    pub capacity: usize,
    /// Optional sink callback invoked synchronously on each flushed batch.
    pub sink: Option<TelemetryDigestSinkFn>,
}

impl Default for ModTelemetrySinkPlugin {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for ModTelemetrySinkPlugin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ModTelemetrySinkPlugin")
            .field("capacity", &self.capacity)
            .field("has_sink", &self.sink.is_some())
            .finish()
    }
}

impl ModTelemetrySinkPlugin {
    /// Creates a [`ModTelemetrySinkPlugin`] with [`DEFAULT_TELEMETRY_RING_CAPACITY`].
    #[inline]
    pub fn new() -> Self {
        Self {
            capacity: DEFAULT_TELEMETRY_RING_CAPACITY,
            sink: None,
        }
    }

    /// Configures the [`ModTelemetryRing`] retention capacity (`clamped >= 1`).
    #[inline]
    #[must_use]
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity.max(1);
        self
    }

    /// Attaches a streaming [`TelemetryDigestSinkFn`] invoked whenever telemetry frames flush.
    #[inline]
    #[must_use]
    pub fn with_sink<F>(mut self, sink: F) -> Self
    where
        F: Fn(&[SystemTelemetryDigest64]) + Send + Sync + 'static,
    {
        self.sink = Some(Arc::new(sink));
        self
    }
}

impl Plugin for ModTelemetrySinkPlugin {
    fn build(&self, app: &mut App) {
        ensure_budget_resources(app.world_mut());
        if let Some(telemetry) = app.world().get_resource::<ModBudgetTelemetry>() {
            telemetry.set_observing(true);
        }
        app.insert_resource(ModTelemetryRing::with_capacity(self.capacity));
        app.init_resource::<NativeSystemTelemetryTracker>();
        app.init_resource::<TelemetryFrameSeq>();
        if let Some(ref sink) = self.sink {
            app.insert_resource(ModTelemetrySinkCallback(Arc::clone(sink)));
        }
        app.add_systems(Last, flush_mod_telemetry_digests.after(flush_mod_budgets));
    }
}
