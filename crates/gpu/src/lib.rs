//! A GPU backend for graphbuff's kernels, on [`wgpu`]: Vulkan, Direct3D 12
//! or Metal, whichever the machine has.
//!
//! [`WgpuKernels`] implements [`Kernels`]. The graph-wide kernels run on the GPU: walk
//! counts ([`Kernels::propagate`]), [`Kernels::bfs`], [`Kernels::wcc`],
//! [`Kernels::triangles`] and, on devices with 64-bit floats, [`Kernels::page_rank`].
//! Their results are the CPU's exactly: integer work is order-independent, and PageRank
//! sums each node's shares on the GPU in the CPU's order, with additions only (divisions
//! and the damping step stay on the CPU). The rest (expand, filter, select, aggregates,
//! Top-N) is one pass over data that lives in host memory, where copying it to the GPU
//! costs as much as the pass, so it runs on [`CpuKernels`]; so does any kernel below
//! [`WgpuKernels::with_min_work`] relationships, and any the device cannot hold. A device
//! error (out of memory, a lost device) turns the backend off and runs everything on the
//! CPU from then on.
//!
//! The CSR adjacency is copied to the GPU once per export ([`Csr::id`]), a layout (type and
//! direction) at a time as kernels first need it; the two most recent exports stay.

mod shaders;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};

use graphbuff_core::{
    Aggregates, CmpOp, Column, CpuKernels, Csr, Direction, Error, ExpandBatch, Hop, Kernels, Mask, Result, Value, VectorRows,
    VectorScores, VectorSimilarity,
};
use wgpu::util::DeviceExt;

/// Relationships a kernel must read before it runs on the GPU, by default.
pub const DEFAULT_MIN_WORK: usize = 1 << 18;

/// BFS levels or WCC rounds recorded per submission between checks for convergence.
const BATCH: u32 = 8;

/// Triangle work (row entries read) per submission, so that no submission runs long
/// enough for the operating system to reset the device.
const TRIANGLE_SLICE: u64 = 1 << 26;

/// Exports kept on the device.
const KEPT_EXPORTS: usize = 2;

/// Buffers kept for reuse between kernel calls.
const POOLED: usize = 16;

/// The usage of device copies of vector rows: a patched copy is copied from an earlier one.
const VECTOR_ROWS: wgpu::BufferUsages = wgpu::BufferUsages::STORAGE
    .union(wgpu::BufferUsages::COPY_SRC)
    .union(wgpu::BufferUsages::COPY_DST);

/// The usage of buffers a kernel call reads, writes and copies out.
const SCRATCH: wgpu::BufferUsages = wgpu::BufferUsages::STORAGE
    .union(wgpu::BufferUsages::COPY_SRC)
    .union(wgpu::BufferUsages::COPY_DST);

/// Kernels on a GPU through `wgpu`, with [`CpuKernels`] for the rest.
pub struct WgpuKernels {
    cpu: CpuKernels,
    gpu: Gpu,
    min_work: usize,
    gpu_runs: AtomicU64,
}

impl std::fmt::Debug for WgpuKernels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WgpuKernels").field("adapter", &self.adapter()).field("min_work", &self.min_work).finish()
    }
}

/// The device, its pipelines and the exports copied to it.
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    info: wgpu::AdapterInfo,
    max_binding: u64,
    propagate: wgpu::ComputePipeline,
    propagate_heavy: wgpu::ComputePipeline,
    mask: wgpu::ComputePipeline,
    bfs: wgpu::ComputePipeline,
    wcc_hook: wgpu::ComputePipeline,
    wcc_jump: wgpu::ComputePipeline,
    triangles: wgpu::ComputePipeline,
    /// `None` without 64-bit float support.
    page_rank: Option<wgpu::ComputePipeline>,
    vectors: wgpu::ComputePipeline,
    vector_patch: wgpu::ComputePipeline,
    exports: Mutex<Vec<(u64, Arc<DeviceCsr>)>>,
    /// Vector rows copied to the device, by [`VectorRows::id`], most recent last.
    vector_rows: Mutex<Vec<(u64, Arc<wgpu::Buffer>)>>,
    /// Buffers a kernel call is done with, for the next to reuse (allocating a large
    /// buffer, and the first touch of its pages, cost more than the kernels do).
    pool: Mutex<Vec<wgpu::Buffer>>,
    /// Set by the first device error; the backend runs on the CPU from then on.
    failed: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}

/// The layouts of one export on the device, by (type, outgoing).
#[derive(Default)]
struct DeviceCsr {
    layouts: Mutex<HashMap<(u32, bool), Arc<Layout>>>,
}

/// One CSR layout on the device.
struct Layout {
    offsets: wgpu::Buffer,
    targets: wgpu::Buffer,
    /// The rows longer than `HEAVY` entries, and how many there are.
    heavy: wgpu::Buffer,
    heavy_rows: u32,
}

/// Rows longer than this many entries are summed by a whole workgroup (`HEAVY` in the
/// walk shader).
const HEAVY: u32 = 256;

/// A result copied back from the device, mapped: unmapped and returned to the pool when
/// dropped.
struct Readback<'g> {
    gpu: &'g Gpu,
    buffer: Option<wgpu::Buffer>,
    len: usize,
}

impl Readback<'_> {
    /// The bytes as values of `T`, in one copy.
    fn get<T: bytemuck::Pod>(&self) -> Vec<T> {
        let b = self.buffer.as_ref().expect("mapped");
        bytemuck::pod_collect_to_vec(&b.get_mapped_range(..).expect("mapped")[..self.len])
    }

    /// The first 32-bit word (a flag or a counter).
    fn word(&self) -> u32 {
        self.get::<u32>()[0]
    }
}

impl Drop for Readback<'_> {
    fn drop(&mut self) {
        if let Some(b) = self.buffer.take() {
            b.unmap();
            self.gpu.recycle([b]);
        }
    }
}

/// A layout a pulling kernel reads: which one, and whether to skip self-loops in it.
struct Pull {
    layout: Arc<Layout>,
    skip_self: bool,
}

impl WgpuKernels {
    /// The highest-performance adapter the system offers. `Unsupported` if there is none,
    /// or it cannot run the kernels.
    pub fn new() -> Result<WgpuKernels> {
        Ok(WgpuKernels {
            cpu: CpuKernels::new(),
            gpu: Gpu::new()?,
            min_work: DEFAULT_MIN_WORK,
            gpu_runs: AtomicU64::new(0),
        })
    }

    /// Kernels reading fewer than `min_work` relationships run on the CPU (`0`: every
    /// GPU kernel runs on the GPU).
    pub fn with_min_work(mut self, min_work: usize) -> WgpuKernels {
        self.min_work = min_work;
        self
    }

    /// The CPU backend used for the other kernels.
    pub fn with_cpu(mut self, cpu: CpuKernels) -> WgpuKernels {
        self.cpu = cpu;
        self
    }

    /// The adapter: its name and graphics API, e.g. `"AMD Radeon RX 5700 XT (Vulkan)"`.
    pub fn adapter(&self) -> String {
        format!("{} ({:?})", self.gpu.info.name, self.gpu.info.backend)
    }

    /// Whether PageRank runs on the GPU (the device has 64-bit floats).
    pub fn has_f64(&self) -> bool {
        self.gpu.page_rank.is_some()
    }

    /// Kernel calls that ran on the GPU so far.
    pub fn gpu_runs(&self) -> u64 {
        self.gpu_runs.load(Relaxed)
    }

    /// The device error that turned the backend off, if any.
    pub fn error(&self) -> Option<String> {
        self.gpu.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether a kernel reading `work` relationships goes to the GPU.
    fn on_gpu(&self, work: usize) -> bool {
        work >= self.min_work && !self.gpu.failed.load(Relaxed)
    }

    fn ran<T>(&self, r: Option<T>) -> Option<T> {
        if r.is_some() {
            self.gpu_runs.fetch_add(1, Relaxed);
        }
        r
    }
}

/// Relationships a pull in `dir` over `types` reads.
fn pull_work(csr: &Csr, dir: Direction, types: &[u32]) -> usize {
    let (out, inc) = layouts(dir);
    types
        .iter()
        .map(|&t| {
            usize::from(out) * csr.adjacency(t, Direction::Outgoing).len()
                + usize::from(inc) * csr.adjacency(t, Direction::Incoming).len()
        })
        .sum()
}

/// Which layouts one hop in `dir` reads: (outgoing, incoming).
fn layouts(dir: Direction) -> (bool, bool) {
    (matches!(dir, Direction::Outgoing | Direction::Both), matches!(dir, Direction::Incoming | Direction::Both))
}

impl Kernels for WgpuKernels {
    fn name(&self) -> &str {
        "wgpu"
    }

    fn expand(&self, csr: &Csr, sources: &[u32], dir: Direction, types: &[u32]) -> ExpandBatch {
        self.cpu.expand(csr, sources, dir, types)
    }

    fn propagate(&self, csr: &Csr, counts: &[u64], dir: Direction, types: &[u32]) -> Result<Vec<u64>> {
        assert_eq!(counts.len(), csr.node_count(), "one count per node");
        if self.on_gpu(pull_work(csr, dir.reverse(), types))
            && let Some(r) = self.ran(self.gpu.propagate(csr, counts, dir, types))
        {
            return r;
        }
        self.cpu.propagate(csr, counts, dir, types)
    }

    fn walk(&self, csr: &Csr, counts: &[u64], hops: &[Hop<'_>]) -> Result<Vec<u64>> {
        assert_eq!(counts.len(), csr.node_count(), "one count per node");
        let work = hops.iter().map(|h| pull_work(csr, h.dir.reverse(), h.types)).sum();
        if self.on_gpu(work)
            && let Some(r) = self.ran(self.gpu.walk(csr, counts, hops))
        {
            return r;
        }
        let mut counts = counts.to_vec();
        for hop in hops {
            counts = self.cpu.propagate(csr, &counts, hop.dir, hop.types)?;
            if let Some(m) = hop.mask {
                for (x, c) in counts.iter_mut().enumerate() {
                    if !m.get(x as u32) {
                        *c = 0;
                    }
                }
            }
        }
        Ok(counts)
    }

    fn bfs(&self, csr: &Csr, sources: &[u32], dir: Direction, types: &[u32], max_depth: u32) -> Vec<u32> {
        if self.on_gpu(pull_work(csr, dir.reverse(), types))
            && let Some(r) = self.ran(self.gpu.bfs(csr, sources, dir, types, max_depth))
        {
            return r;
        }
        self.cpu.bfs(csr, sources, dir, types, max_depth)
    }

    fn filter(&self, column: &Column, op: CmpOp, value: &Value) -> Mask {
        self.cpu.filter(column, op, value)
    }

    fn select(&self, rows: &[u32], mask: &Mask) -> Vec<u32> {
        self.cpu.select(rows, mask)
    }

    fn aggregate(&self, column: &Column, rows: &[u32]) -> Aggregates {
        self.cpu.aggregate(column, rows)
    }

    fn aggregate_counts(&self, column: &Column, counts: &[u64]) -> Result<Aggregates> {
        self.cpu.aggregate_counts(column, counts)
    }

    fn top_n(&self, column: &Column, rows: &[u32], n: usize, descending: bool) -> Vec<usize> {
        self.cpu.top_n(column, rows, n, descending)
    }

    fn page_rank(
        &self,
        csr: &Csr,
        types: &[u32],
        dir: Direction,
        damping: f64,
        max_iterations: u32,
        tolerance: f64,
    ) -> (Vec<f64>, u32) {
        if self.gpu.page_rank.is_some()
            && self.on_gpu(pull_work(csr, dir.reverse(), types))
            && let Some(r) = self.ran(self.gpu.page_rank(csr, types, dir, damping, max_iterations, tolerance))
        {
            return r;
        }
        self.cpu.page_rank(csr, types, dir, damping, max_iterations, tolerance)
    }

    fn wcc(&self, csr: &Csr, types: &[u32]) -> Vec<u32> {
        if self.on_gpu(pull_work(csr, Direction::Outgoing, types))
            && let Some(r) = self.ran(self.gpu.wcc(csr, types))
        {
            return r;
        }
        self.cpu.wcc(csr, types)
    }

    fn vector_scores(&self, rows: &VectorRows, query: &[f64], similarity: VectorSimilarity) -> VectorScores {
        // f32 sums cannot hold squared distances near f32::MAX: those stay on the CPU.
        let query_norm = query.iter().map(|x| x * x).sum::<f64>().sqrt();
        let in_range = (rows.max_norm() + query_norm).powi(2) < 1e30;
        if in_range && self.on_gpu(rows.data().len()) {
            let scores = self.ran(self.gpu.vector_scores(rows, query, similarity));
            if let Some(values) = scores {
                return VectorScores { values, f32_terms: rows.dims() as u32 };
            }
        }
        self.cpu.vector_scores(rows, query, similarity)
    }

    fn triangles(&self, csr: &Csr, types: &[u32]) -> Vec<u64> {
        if self.on_gpu(pull_work(csr, Direction::Outgoing, types)) {
            let (offsets, targets) = self.cpu.oriented_neighbours(csr, types);
            if let Some(r) = self.ran(self.gpu.triangles(csr.node_count(), &offsets, &targets)) {
                return r;
            }
            return cpu_triangles(csr.node_count(), &offsets, &targets);
        }
        self.cpu.triangles(csr, types)
    }
}

/// Triangles over oriented neighbour lists on the calling thread (the GPU failed after
/// the lists were built).
fn cpu_triangles(n: usize, offsets: &[usize], targets: &[u32]) -> Vec<u64> {
    let row = |u: usize| &targets[offsets[u]..offsets[u + 1]];
    let mut counts = vec![0u64; n];
    for u in 0..n {
        let fu = row(u);
        for &v in fu {
            let fv = row(v as usize);
            let (mut i, mut j) = (0, 0);
            while i < fu.len() && j < fv.len() {
                match fu[i].cmp(&fv[j]) {
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                    std::cmp::Ordering::Equal => {
                        counts[u] += 1;
                        counts[v as usize] += 1;
                        counts[fu[i] as usize] += 1;
                        i += 1;
                        j += 1;
                    }
                }
            }
        }
    }
    counts
}

/// The hops of type `t` from node `i` in `dir` (self-loops once under `Both`), as
/// `CpuKernels` counts them for PageRank.
fn hop_degree(csr: &Csr, t: u32, dir: Direction, i: u32) -> u32 {
    let o = csr.adjacency(t, Direction::Outgoing).neighbors(i);
    let n = csr.adjacency(t, Direction::Incoming).degree(i) as u32;
    match dir {
        Direction::Outgoing => o.len() as u32,
        Direction::Incoming => n,
        Direction::Both => o.len() as u32 + n - o.iter().filter(|&&x| x == i).count() as u32,
    }
}

// ---------------------------------------------------------------------------------------
// The device
// ---------------------------------------------------------------------------------------

/// Runs a future to completion on this thread (wgpu's native futures need no reactor).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn unsupported(m: impl std::fmt::Display) -> Error {
    Error::Unsupported(m.to_string())
}

fn bytes<T: bytemuck::Pod>(v: &[T]) -> &[u8] {
    bytemuck::cast_slice(v)
}

impl Gpu {
    fn new() -> Result<Gpu> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|e| unsupported(format!("no GPU adapter: {e}")))?;
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            return Err(unsupported(format!("the only adapter, {}, is a software renderer", info.name)));
        }
        let limits = adapter.limits();
        let f64 = adapter.features().contains(wgpu::Features::SHADER_F64);
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("graphbuff"),
            required_features: if f64 { wgpu::Features::SHADER_F64 } else { wgpu::Features::empty() },
            required_limits: wgpu::Limits {
                max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
                max_buffer_size: limits.max_buffer_size,
                max_storage_buffers_per_shader_stage: limits.max_storage_buffers_per_shader_stage.min(8),
                ..wgpu::Limits::downlevel_defaults()
            },
            ..Default::default()
        }))
        .map_err(|e| unsupported(format!("cannot open {}: {e}", info.name)))?;
        let failed = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        {
            let (failed, error) = (failed.clone(), error.clone());
            device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                failed.store(true, Relaxed);
                error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| e.to_string());
            }));
        }
        let pipeline = |src: String, entry: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(entry),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: None,
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let gpu = Gpu {
            propagate: pipeline(shaders::propagate(), "main"),
            propagate_heavy: pipeline(shaders::propagate(), "heavy"),
            mask: pipeline(shaders::mask(), "main"),
            bfs: pipeline(shaders::bfs(), "main"),
            wcc_hook: pipeline(shaders::wcc(), "hook"),
            wcc_jump: pipeline(shaders::wcc(), "jump"),
            triangles: pipeline(shaders::triangles(), "main"),
            page_rank: f64.then(|| pipeline(shaders::page_rank(), "main")),
            vectors: pipeline(shaders::vectors(), "main"),
            vector_patch: pipeline(shaders::vector_patch(), "main"),
            vector_rows: Mutex::new(Vec::new()),
            max_binding: limits.max_storage_buffer_binding_size.min(limits.max_buffer_size),
            device,
            queue,
            info,
            exports: Mutex::new(Vec::new()),
            pool: Mutex::new(Vec::new()),
            failed,
            error,
        };
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(unsupported)?;
        if gpu.failed.load(Relaxed) {
            let e = gpu.error.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
            return Err(unsupported(format!("cannot build the GPU kernels: {e}")));
        }
        Ok(gpu)
    }

    fn ok(&self) -> bool {
        !self.failed.load(Relaxed)
    }

    /// A storage buffer holding `data` that lives as long as the export (a layout), or
    /// `None` if it is too large.
    fn upload(&self, label: &str, data: &[u8]) -> Option<wgpu::Buffer> {
        if data.len() as u64 > self.max_binding {
            return None;
        }
        let pad = [0u8; 4];
        let contents = if data.is_empty() { &pad[..] } else { data };
        Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents,
            usage: wgpu::BufferUsages::STORAGE,
        }))
    }

    /// [`upload`](Self::upload) with `usage`.
    fn upload_with(&self, label: &str, data: &[u8], usage: wgpu::BufferUsages) -> Option<wgpu::Buffer> {
        if data.len() as u64 > self.max_binding {
            return None;
        }
        let pad = [0u8; 4];
        let contents = if data.is_empty() { &pad[..] } else { data };
        Some(self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents, usage }))
    }

    /// A buffer of at least `len` bytes with `usage`, from the pool if it has one.
    fn take(&self, label: &str, len: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
        let size = len.max(4).next_multiple_of(4);
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = pool.iter().position(|b| b.size() == size && b.usage() == usage) {
            return pool.swap_remove(k);
        }
        drop(pool);
        self.device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size, usage, mapped_at_creation: false })
    }

    /// Returns buffers to the pool, which keeps the most recent `POOLED`.
    fn recycle(&self, buffers: impl IntoIterator<Item = wgpu::Buffer>) {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        pool.extend(buffers);
        let extra = pool.len().saturating_sub(POOLED);
        pool.drain(..extra);
    }

    /// A storage buffer for one kernel call holding `data`, or `None` if it is too large.
    /// The copy is queued: it happens before work submitted later.
    fn input(&self, label: &str, data: &[u8]) -> Option<wgpu::Buffer> {
        if data.len() as u64 > self.max_binding {
            return None;
        }
        let b = self.take(label, data.len() as u64, SCRATCH);
        if !data.is_empty() {
            self.queue.write_buffer(&b, 0, data);
        }
        Some(b)
    }

    /// A zeroed storage buffer for one kernel call of `len` bytes (at least 4), or `None`
    /// if it is too large. The clearing is queued: it happens before work submitted later.
    fn zeroed(&self, label: &str, len: u64) -> Option<wgpu::Buffer> {
        if len > self.max_binding {
            return None;
        }
        let b = self.take(label, len, SCRATCH);
        let mut enc = self.encoder();
        enc.clear_buffer(&b, 0, None);
        self.queue.submit([enc.finish()]);
        Some(b)
    }

    fn params(&self, n: u32, arg: u32, skip_self: bool, slot: u32) -> wgpu::Buffer {
        self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("params"),
            contents: bytes(&[n, arg, u32::from(skip_self), slot]),
            usage: wgpu::BufferUsages::UNIFORM,
        })
    }

    /// The device copy of `csr`'s layout of type `t` (outgoing or incoming).
    fn layout(&self, csr: &Csr, t: u32, out: bool) -> Option<Arc<Layout>> {
        let export = {
            let mut exports = self.exports.lock().unwrap_or_else(|e| e.into_inner());
            let e = match exports.iter().position(|(id, _)| *id == csr.id()) {
                Some(k) => exports.remove(k).1,
                None => Arc::new(DeviceCsr::default()),
            };
            exports.push((csr.id(), e.clone()));
            if exports.len() > KEPT_EXPORTS {
                exports.remove(0);
            }
            e
        };
        let mut layouts = export.layouts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = layouts.get(&(t, out)) {
            return Some(l.clone());
        }
        let a = csr.adjacency(t, if out { Direction::Outgoing } else { Direction::Incoming });
        let heavy: Vec<u32> = (0..csr.node_count() as u32).filter(|&i| a.degree(i) > HEAVY as usize).collect();
        let l = Arc::new(Layout {
            offsets: self.upload("offsets", bytes(a.offsets()))?,
            targets: self.upload("targets", bytes(a.targets()))?,
            heavy: self.upload("heavy rows", bytes(&heavy))?,
            heavy_rows: heavy.len() as u32,
        });
        layouts.insert((t, out), l.clone());
        Some(l)
    }

    /// The layouts a node pulls from for a hop in `dir` over `types`, in the CPU's order:
    /// per type, outgoing then incoming; under `Both`, self-loops only on the outgoing side.
    fn pulls(&self, csr: &Csr, dir: Direction, types: &[u32]) -> Option<Vec<Pull>> {
        let (out, inc) = layouts(dir.reverse());
        let mut pulls = Vec::new();
        for &t in types {
            if out {
                pulls.push(Pull { layout: self.layout(csr, t, true)?, skip_self: false });
            }
            if inc {
                pulls.push(Pull { layout: self.layout(csr, t, false)?, skip_self: dir == Direction::Both });
            }
        }
        Some(pulls)
    }

    /// Records one dispatch of `pipeline` over `n` invocations.
    fn dispatch(
        &self,
        enc: &mut wgpu::CommandEncoder,
        pipeline: &wgpu::ComputePipeline,
        params: &wgpu::Buffer,
        buffers: &[&wgpu::Buffer],
        n: u32,
    ) {
        if n == 0 {
            return;
        }
        let mut entries = vec![wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() }];
        entries.extend(
            buffers
                .iter()
                .enumerate()
                .map(|(k, b)| wgpu::BindGroupEntry { binding: k as u32 + 1, resource: b.as_entire_binding() }),
        );
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &entries,
        });
        let groups = n.div_ceil(256);
        let x = groups.min(65_535);
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(x, groups.div_ceil(x), 1);
    }

    fn encoder(&self) -> wgpu::CommandEncoder {
        self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default())
    }

    /// Submits `enc` after copying the first `len` bytes of each of `reads` out, waits,
    /// and returns them, mapped; `None` after a device error.
    fn finish(&self, mut enc: wgpu::CommandEncoder, reads: &[(&wgpu::Buffer, u64)]) -> Option<Vec<Readback<'_>>> {
        let staging: Vec<(wgpu::Buffer, u64)> = reads
            .iter()
            .map(|&(src, len)| {
                let usage = wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST;
                let b = self.take("readback", len, usage);
                enc.copy_buffer_to_buffer(src, 0, &b, 0, len.max(4).next_multiple_of(4));
                (b, len)
            })
            .collect();
        self.queue.submit([enc.finish()]);
        let (tx, rx) = mpsc::channel();
        for (k, (b, _)) in staging.iter().enumerate() {
            let tx = tx.clone();
            b.map_async(wgpu::MapMode::Read, .., move |r| {
                let _ = tx.send((k, r.is_ok()));
            });
        }
        drop(tx);
        if self.device.poll(wgpu::PollType::wait_indefinitely()).is_err() {
            self.fail("waiting for the device failed");
            return None;
        }
        let mapped = rx.iter().take(staging.len()).all(|(_, ok)| ok);
        if !mapped || !self.ok() {
            self.fail("reading a result back failed");
            return None;
        }
        Some(staging.into_iter().map(|(b, len)| Readback { gpu: self, buffer: Some(b), len: len as usize }).collect())
    }

    /// The device copy of `rows` (the last [`KEPT_EXPORTS`] are kept), or `None` if it is
    /// too large.
    fn vector_rows(&self, rows: &VectorRows) -> Option<Arc<wgpu::Buffer>> {
        let mut kept = self.vector_rows.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = kept.iter().position(|(id, _)| *id == rows.id()) {
            let entry = kept.remove(k);
            let b = entry.1.clone();
            kept.push(entry);
            return Some(b);
        }
        // A copy of an earlier value, brought up to date with the rows changed since.
        let earlier = rows
            .changed_since()
            .find_map(|(id, at)| kept.iter().find(|(k, _)| *k == id).map(|(_, b)| (b.clone(), at)));
        let b = match earlier {
            Some((earlier, at)) => self.patched_rows(&earlier, rows, at)?,
            None => {
                // Column by column (see the shader).
                let (n, d) = (rows.len(), rows.dims());
                let mut columns = vec![0f32; n * d];
                for (i, row) in rows.data().chunks_exact(d).enumerate() {
                    for (k, x) in row.iter().enumerate() {
                        columns[k * n + i] = *x;
                    }
                }
                self.upload_with("vector rows", bytes(&columns), VECTOR_ROWS)?
            }
        };
        let b = Arc::new(b);
        kept.push((rows.id(), b.clone()));
        if kept.len() > KEPT_EXPORTS {
            kept.remove(0);
        }
        Some(b)
    }

    /// A device copy of `rows` made from `earlier`, the copy of a value it was patched
    /// from: copied on the device, then the rows at `at` written.
    fn patched_rows(&self, earlier: &wgpu::Buffer, rows: &VectorRows, at: &[u32]) -> Option<wgpu::Buffer> {
        let (n, d) = (rows.len(), rows.dims());
        let len = (n * d * 4) as u64;
        if len > self.max_binding || earlier.size() != len.max(4) {
            return None;
        }
        let b = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vector rows"),
            size: len,
            usage: VECTOR_ROWS,
            mapped_at_creation: false,
        });
        let mut values = Vec::with_capacity(at.len() * d);
        for &r in at {
            values.extend_from_slice(&rows.data()[r as usize * d..(r as usize + 1) * d]);
        }
        let at_buf = self.input("vector patch rows", bytes(at))?;
        let values_buf = self.input("vector patch values", bytes(&values))?;
        let writes = (at.len() * d) as u32;
        let params = self.params(writes, d as u32, false, n as u32);
        let mut enc = self.encoder();
        enc.copy_buffer_to_buffer(earlier, 0, &b, 0, len);
        self.dispatch(&mut enc, &self.vector_patch, &params, &[&b, &at_buf, &values_buf], writes);
        self.queue.submit([enc.finish()]);
        // Later work runs after this submission, so the buffers can be reused.
        self.recycle([at_buf, values_buf]);
        Some(b)
    }

    fn vector_scores(&self, rows: &VectorRows, query: &[f64], similarity: VectorSimilarity) -> Option<Vec<f64>> {
        let n = rows.len();
        if n == 0 {
            return Some(Vec::new());
        }
        let device_rows = self.vector_rows(rows)?;
        let q: Vec<f32> = query.iter().map(|x| *x as f32).collect();
        let query_buf = self.input("vector query", bytes(&q))?;
        let out = self.zeroed("vector scores", n as u64 * 4)?;
        let slot = match similarity {
            VectorSimilarity::Cosine => 0,
            VectorSimilarity::Euclidean => 1,
        };
        let params = self.params(n as u32, rows.dims() as u32, false, slot);
        let mut enc = self.encoder();
        self.dispatch(&mut enc, &self.vectors, &params, &[&device_rows, &query_buf, &out], n as u32);
        let read = self.finish(enc, &[(&out, n as u64 * 4)])?;
        let values: Vec<f32> = read[0].get();
        drop(read);
        self.recycle([query_buf, out]);
        Some(values.into_iter().map(f64::from).collect())
    }

    fn fail(&self, m: &str) {
        self.failed.store(true, Relaxed);
        self.error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| m.to_string());
    }

    fn propagate(&self, csr: &Csr, counts: &[u64], dir: Direction, types: &[u32]) -> Option<Result<Vec<u64>>> {
        self.walk(csr, counts, &[Hop { dir, types, mask: None }])
    }

    /// Walk counts over `hops`, kept on the device between hops (two buffers in turn).
    fn walk(&self, csr: &Csr, counts: &[u64], hops: &[Hop<'_>]) -> Option<Result<Vec<u64>>> {
        let n = csr.node_count();
        let len = n as u64 * 8;
        let mut cur = self.input("counts", bytes(counts))?;
        let mut next = self.zeroed("counts", len)?;
        let flags = self.zeroed("flags", 4)?;
        let mut enc = self.encoder();
        let mut keep = Vec::new(); // masks and parameters, alive until submitted
        for hop in hops {
            enc.clear_buffer(&next, 0, None);
            for p in self.pulls(csr, hop.dir, hop.types)? {
                let l = &p.layout;
                let params = self.params(n as u32, 0, p.skip_self, 0);
                let bufs = [&l.offsets, &l.targets, &cur, &next, &flags, &l.heavy];
                self.dispatch(&mut enc, &self.propagate, &params, &bufs[..5], n as u32);
                if l.heavy_rows > 0 {
                    // A workgroup per heavy row: `dispatch` counts invocations.
                    let params = self.params(l.heavy_rows, 0, p.skip_self, 0);
                    self.dispatch(&mut enc, &self.propagate_heavy, &params, &bufs, l.heavy_rows * 256);
                }
            }
            if let Some(m) = hop.mask {
                let mask = self.input("mask", bytes(m.words()))?;
                let params = self.params(n as u32, 0, false, 0);
                self.dispatch(&mut enc, &self.mask, &params, &[&mask, &next], n as u32);
                keep.push(mask);
            }
            std::mem::swap(&mut cur, &mut next);
        }
        let out = self.finish(enc, &[(&cur, len), (&flags, 4)])?;
        self.recycle(keep.into_iter().chain([cur, next, flags]));
        if out[1].word() != 0 {
            return Some(Err(unsupported("a walk count overflows a 64-bit integer; aggregate the rows instead")));
        }
        Some(Ok(out[0].get()))
    }

    fn bfs(&self, csr: &Csr, sources: &[u32], dir: Direction, types: &[u32], max_depth: u32) -> Option<Vec<u32>> {
        let n = csr.node_count();
        let mut depth = vec![u32::MAX; n];
        for &s in sources {
            depth[s as usize] = 0;
        }
        // Pull: a node is reached from the nodes a hop in `dir` comes from. Self-loops
        // never reach an unreached node, so none are skipped.
        let pulls = self.pulls(csr, dir, types)?;
        let depth_buf = self.input("depth", bytes(&depth))?;
        let mut level = 0;
        while level < max_depth {
            let batch = BATCH.min(max_depth - level);
            let changed = self.zeroed("changed", u64::from(batch) * 4)?;
            let mut enc = self.encoder();
            for b in 0..batch {
                // Each level counts the nodes it reaches into its own slot.
                let params = self.params(n as u32, level + b + 1, false, b);
                for p in &pulls {
                    let bufs = [&p.layout.offsets, &p.layout.targets, &depth_buf, &changed];
                    self.dispatch(&mut enc, &self.bfs, &params, &bufs, n as u32);
                }
            }
            let out = self.finish(enc, &[(&changed, u64::from(batch) * 4)])?;
            let per_level: Vec<u32> = out[0].get();
            level += batch;
            if per_level.contains(&0) {
                break; // a level reached no node: so will every later one
            }
        }
        let enc = self.encoder();
        let out = self.finish(enc, &[(&depth_buf, n as u64 * 4)])?;
        Some(out[0].get())
    }

    fn page_rank(
        &self,
        csr: &Csr,
        types: &[u32],
        dir: Direction,
        damping: f64,
        max_iterations: u32,
        tolerance: f64,
    ) -> Option<(Vec<f64>, u32)> {
        let pipeline = self.page_rank.as_ref()?;
        let n = csr.node_count();
        let base = 1.0 - damping;
        let degree: Vec<u32> =
            (0..n as u32).map(|i| types.iter().map(|&t| hop_degree(csr, t, dir, i)).sum()).collect();
        let pulls = self.pulls(csr, dir, types)?;
        let share_buf = self.zeroed("share", n as u64 * 8)?;
        let sum = self.zeroed("sum", n as u64 * 8)?;
        let mut score = vec![base; n];
        let mut iterations = 0;
        // As `CpuKernels::page_rank`, with each node's sum of shares taken on the device.
        while iterations < max_iterations {
            iterations += 1;
            let share: Vec<f64> =
                score.iter().zip(&degree).map(|(&s, &d)| if d == 0 { 0.0 } else { s / f64::from(d) }).collect();
            if n > 0 {
                self.queue.write_buffer(&share_buf, 0, bytes(&share));
            }
            let mut enc = self.encoder();
            enc.clear_buffer(&sum, 0, None);
            for p in &pulls {
                let params = self.params(n as u32, 0, p.skip_self, 0);
                let bufs = [&p.layout.offsets, &p.layout.targets, &share_buf, &sum];
                self.dispatch(&mut enc, pipeline, &params, &bufs, n as u32);
            }
            let out = self.finish(enc, &[(&sum, n as u64 * 8)])?;
            let sums: Vec<f64> = out[0].get();
            let next: Vec<f64> = sums.iter().map(|&s| base + damping * s).collect();
            let moved = next.iter().zip(&score).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            score = next;
            if moved <= tolerance {
                break;
            }
        }
        Some((score, iterations))
    }

    fn wcc(&self, csr: &Csr, types: &[u32]) -> Option<Vec<u32>> {
        let n = csr.node_count();
        let outs: Vec<Arc<Layout>> = types.iter().map(|&t| self.layout(csr, t, true)).collect::<Option<_>>()?;
        let init: Vec<u32> = (0..n as u32).collect();
        let labels = self.input("labels", bytes(&init))?;
        let changed = self.zeroed("changed", 4)?;
        let params = self.params(n as u32, 0, false, 0);
        loop {
            let mut enc = self.encoder();
            enc.clear_buffer(&changed, 0, None);
            for _ in 0..BATCH {
                for l in &outs {
                    let bufs = [&l.offsets, &l.targets, &labels, &changed];
                    self.dispatch(&mut enc, &self.wcc_hook, &params, &bufs, n as u32);
                }
                self.dispatch_jump(&mut enc, &params, &labels, &changed, n as u32);
            }
            let out = self.finish(enc, &[(&changed, 4)])?;
            if out[0].word() == 0 {
                break;
            }
        }
        let out = self.finish(self.encoder(), &[(&labels, n as u64 * 4)])?;
        Some(out[0].get())
    }

    /// The pointer-jumping pass of WCC. Its pipeline's layout has only the parameters,
    /// the labels and the flag (an automatic layout leaves out what its entry point does
    /// not use).
    fn dispatch_jump(
        &self,
        enc: &mut wgpu::CommandEncoder,
        params: &wgpu::Buffer,
        labels: &wgpu::Buffer,
        changed: &wgpu::Buffer,
        n: u32,
    ) {
        if n == 0 {
            return;
        }
        let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.wcc_jump.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: labels.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: changed.as_entire_binding() },
            ],
        });
        let groups = n.div_ceil(256);
        let x = groups.min(65_535);
        let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&self.wcc_jump);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(x, groups.div_ceil(x), 1);
    }

    fn triangles(&self, n: usize, offsets: &[usize], targets: &[u32]) -> Option<Vec<u64>> {
        let offs: Vec<u32> = offsets.iter().map(|&o| u32::try_from(o).ok()).collect::<Option<_>>()?;
        let off_buf = self.upload("offsets", bytes(&offs))?;
        let tgt_buf = self.upload("targets", bytes(targets))?;
        let counts = self.zeroed("counts", n as u64 * 4)?;
        let flags = self.zeroed("flags", 4)?;
        let degree = |u: usize| (offsets[u + 1] - offsets[u]) as u64;
        // Node ranges of about `TRIANGLE_SLICE` entries read, one submission each.
        let mut start = 0;
        while start < n {
            let mut work = 0u64;
            let mut end = start;
            while end < n && (end == start || work < TRIANGLE_SLICE) {
                let row = &targets[offsets[end]..offsets[end + 1]];
                work += row.iter().map(|&v| row.len() as u64 + degree(v as usize)).sum::<u64>() + 1;
                end += 1;
            }
            let mut enc = self.encoder();
            let params = self.params((end - start) as u32, start as u32, false, 0);
            let bufs = [&off_buf, &tgt_buf, &counts, &flags];
            self.dispatch(&mut enc, &self.triangles, &params, &bufs, (end - start) as u32);
            self.queue.submit([enc.finish()]);
            if self.device.poll(wgpu::PollType::wait_indefinitely()).is_err() || !self.ok() {
                self.fail("the triangle kernel failed");
                return None;
            }
            start = end;
        }
        let out = self.finish(self.encoder(), &[(&counts, n as u64 * 4), (&flags, 4)])?;
        if out[1].word() != 0 {
            return None; // a count passed 2^32 - 1: the CPU counts in 64 bits
        }
        Some(out[0].get::<u32>().into_iter().map(u64::from).collect())
    }
}
