//! WGSL compute shaders. Every kernel runs one invocation per node (or per node of a
//! range), 256 to a workgroup, over a two-dimensional grid of workgroups (a dimension
//! holds at most 65,535): see `idx`.
//!
//! WGSL has 32-bit integers only, so 64-bit counts are pairs `(low, high)`.

/// Shared by every shader: parameters and the invocation's node index.
const COMMON: &str = r"
struct Params {
    // Invocations (nodes) in this dispatch.
    n: u32,
    // Kernel-specific: the BFS level, the first node of a triangle range.
    arg: u32,
    // 1: skip entries pointing back at the node itself (self-loops under `Both`).
    skip_self: u32,
    // The counter slot a BFS level adds to.
    slot: u32,
}

@group(0) @binding(0) var<uniform> p: Params;

fn idx(wg: vec3<u32>, nwg: vec3<u32>, lid: vec3<u32>) -> u32 {
    return (wg.y * nwg.x + wg.x) * 256u + lid.x;
}
";

/// One hop of walk counts, pulled over one layout: `result[j] += sum(counts[i])` over
/// the entries `i` of row `j`. Counts are 64-bit; an overflow sets `flags[0]`. `main`
/// takes a row per invocation and leaves out rows longer than `HEAVY`; `heavy` takes
/// those (listed in `rows`) a workgroup each, so that a few dense nodes do not keep the
/// whole dispatch waiting on one invocation. Integer sums do not depend on the order.
const PROPAGATE: &str = r"
@group(0) @binding(1) var<storage, read> offsets: array<u32>;
@group(0) @binding(2) var<storage, read> targets: array<u32>;
@group(0) @binding(3) var<storage, read> counts: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read_write> result: array<vec2<u32>>;
@group(0) @binding(5) var<storage, read_write> flags: array<atomic<u32>>;
@group(0) @binding(6) var<storage, read> rows: array<u32>;

const HEAVY: u32 = 256u;

// a + b, and whether it wrapped.
fn add64(a: vec2<u32>, b: vec2<u32>) -> vec3<u32> {
    let lo = a.x + b.x;
    let carry = select(0u, 1u, lo < a.x);
    let hi1 = a.y + b.y;
    let hi = hi1 + carry;
    return vec3<u32>(lo, hi, select(0u, 1u, hi1 < a.y || hi < hi1));
}

// The sum of the entries k0, k0 + step, ... below k1 of row j.
fn row_sum(j: u32, k0: u32, k1: u32, step: u32) -> vec3<u32> {
    var acc = vec3<u32>(0u, 0u, 0u);
    for (var k = k0; k < k1; k = k + step) {
        let i = targets[k];
        if (p.skip_self == 1u && i == j) {
            continue;
        }
        let s = add64(acc.xy, counts[i]);
        acc = vec3<u32>(s.xy, acc.z | s.z);
    }
    return acc;
}

fn store(j: u32, acc: vec3<u32>) {
    let s = add64(result[j], acc.xy);
    result[j] = s.xy;
    if ((acc.z | s.z) != 0u) {
        atomicOr(&flags[0], 1u);
    }
}

@compute @workgroup_size(256)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
    let j = idx(wg, nwg, lid);
    if (j >= p.n) {
        return;
    }
    let start = offsets[j];
    let end = offsets[j + 1u];
    if (end - start > HEAVY) {
        return;
    }
    store(j, row_sum(j, start, end, 1u));
}

var<workgroup> part: array<vec3<u32>, 256>;

@compute @workgroup_size(256)
fn heavy(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
         @builtin(local_invocation_index) li: u32) {
    let r = wg.y * nwg.x + wg.x;
    var acc = vec3<u32>(0u, 0u, 0u);
    var j = 0u;
    if (r < p.n) {
        j = rows[r];
        acc = row_sum(j, offsets[j] + li, offsets[j + 1u], 256u);
    }
    part[li] = acc;
    workgroupBarrier();
    for (var s = 128u; s > 0u; s = s >> 1u) {
        if (li < s) {
            let a = part[li];
            let b = part[li + s];
            let t = add64(a.xy, b.xy);
            part[li] = vec3<u32>(t.xy, a.z | b.z | t.z);
        }
        workgroupBarrier();
    }
    if (li == 0u && r < p.n) {
        store(j, part[0]);
    }
}
";

/// The end of a hop of a walk: nodes whose bit in `mask` is clear get count zero.
const MASK: &str = r"
@group(0) @binding(1) var<storage, read> mask: array<u32>;
@group(0) @binding(2) var<storage, read_write> counts: array<vec2<u32>>;

@compute @workgroup_size(256)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
    let j = idx(wg, nwg, lid);
    if (j >= p.n) {
        return;
    }
    if (((mask[j / 32u] >> (j % 32u)) & 1u) == 0u) {
        counts[j] = vec2<u32>(0u, 0u);
    }
}
";

/// One BFS level, pulled over one layout: an unreached node with an entry at level
/// `arg - 1` gets level `arg`; `changed[slot]` counts such nodes.
const BFS: &str = r"
@group(0) @binding(1) var<storage, read> offsets: array<u32>;
@group(0) @binding(2) var<storage, read> targets: array<u32>;
@group(0) @binding(3) var<storage, read_write> depth: array<u32>;
@group(0) @binding(4) var<storage, read_write> changed: array<atomic<u32>>;

@compute @workgroup_size(256)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
    let j = idx(wg, nwg, lid);
    if (j >= p.n || depth[j] != 0xffffffffu) {
        return;
    }
    let want = p.arg - 1u;
    let end = offsets[j + 1u];
    for (var k = offsets[j]; k < end; k = k + 1u) {
        // A node set to `arg` by another invocation of this level never equals `want`.
        if (depth[targets[k]] == want) {
            depth[j] = p.arg;
            atomicAdd(&changed[p.slot], 1u);
            return;
        }
    }
}
";

/// Weakly connected components by minimum-label propagation over one outgoing layout
/// (`hook`), and pointer jumping (`jump`). Labels only decrease, and each is the index of
/// a node in the same component, so the fixed point is the smallest index of each.
const WCC: &str = r"
@group(0) @binding(1) var<storage, read> offsets: array<u32>;
@group(0) @binding(2) var<storage, read> targets: array<u32>;
@group(0) @binding(3) var<storage, read_write> labels: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> changed: array<atomic<u32>>;

@compute @workgroup_size(256)
fn hook(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
    let x = idx(wg, nwg, lid);
    if (x >= p.n) {
        return;
    }
    let end = offsets[x + 1u];
    for (var k = offsets[x]; k < end; k = k + 1u) {
        let y = targets[k];
        let lx = atomicLoad(&labels[x]);
        let ly = atomicLoad(&labels[y]);
        if (ly < lx) {
            atomicMin(&labels[x], ly);
            atomicStore(&changed[0], 1u);
        } else if (lx < ly) {
            atomicMin(&labels[y], lx);
            atomicStore(&changed[0], 1u);
        }
    }
}

@compute @workgroup_size(256)
fn jump(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
    let x = idx(wg, nwg, lid);
    if (x >= p.n) {
        return;
    }
    let l = atomicLoad(&labels[x]);
    let ll = atomicLoad(&labels[l]);
    if (ll < l) {
        atomicMin(&labels[x], ll);
        atomicStore(&changed[0], 1u);
    }
}
";

/// Triangles over oriented neighbour lists (each undirected edge at its lower-ranked
/// end): for nodes `arg .. arg + n`, every entry `v` of row `u` intersects the rows of `u`
/// and `v`; each common entry is a triangle, counted at its three nodes. A count that
/// wraps sets `flags[0]`.
const TRIANGLES: &str = r"
@group(0) @binding(1) var<storage, read> offsets: array<u32>;
@group(0) @binding(2) var<storage, read> targets: array<u32>;
@group(0) @binding(3) var<storage, read_write> counts: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> flags: array<atomic<u32>>;

fn bump(k: u32) {
    if (atomicAdd(&counts[k], 1u) == 0xffffffffu) {
        atomicOr(&flags[0], 1u);
    }
}

@compute @workgroup_size(256)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
    let local = idx(wg, nwg, lid);
    if (local >= p.n) {
        return;
    }
    let u = p.arg + local;
    let ue = offsets[u + 1u];
    for (var a = offsets[u]; a < ue; a = a + 1u) {
        let v = targets[a];
        var i = offsets[u];
        var j = offsets[v];
        let ve = offsets[v + 1u];
        while (i < ue && j < ve) {
            let x = targets[i];
            let y = targets[j];
            if (x < y) {
                i = i + 1u;
            } else if (x > y) {
                j = j + 1u;
            } else {
                bump(u);
                bump(v);
                bump(x);
                i = i + 1u;
                j = j + 1u;
            }
        }
    }
}
";

/// One PageRank pull over one layout, in 64-bit floats: `sum[j] += share[i]` over the
/// entries `i` of row `j`, in row order. Additions only, so the result is the CPU's bit
/// for bit (IEEE addition is correctly rounded on every device with 64-bit floats).
const PAGE_RANK: &str = r"
@group(0) @binding(1) var<storage, read> offsets: array<u32>;
@group(0) @binding(2) var<storage, read> targets: array<u32>;
@group(0) @binding(3) var<storage, read> share: array<f64>;
@group(0) @binding(4) var<storage, read_write> sum: array<f64>;

@compute @workgroup_size(256)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
    let j = idx(wg, nwg, lid);
    if (j >= p.n) {
        return;
    }
    var s = sum[j];
    let end = offsets[j + 1u];
    for (var k = offsets[j]; k < end; k = k + 1u) {
        let i = targets[k];
        if (p.skip_self == 1u && i == j) {
            continue;
        }
        s = s + share[i];
    }
    sum[j] = s;
}
";

/// Vector scores: per row (`p.n` rows of `p.arg` values, stored column by column so that
/// neighbouring invocations read neighbouring values), the dot product with the query
/// (`p.slot` 0, cosine) or the squared distance to it (`p.slot` 1, euclidean), summed in
/// `f32` in index order.
// Writes rows into vector rows laid out column by column (see VECTORS): `p.n` values,
// `p.arg` per row, the row of each in `at`, `p.slot` rows in all.
const VECTOR_PATCH: &str = r"
@group(0) @binding(1) var<storage, read_write> rows: array<f32>;
@group(0) @binding(2) var<storage, read> at: array<u32>;
@group(0) @binding(3) var<storage, read> values: array<f32>;

@compute @workgroup_size(256)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
    let j = idx(wg, nwg, lid);
    if (j >= p.n) {
        return;
    }
    let d = p.arg;
    let r = j / d;
    let k = j - r * d;
    rows[k * p.slot + at[r]] = values[j];
}
";

const VECTORS: &str = r"
@group(0) @binding(1) var<storage, read> rows: array<f32>;
@group(0) @binding(2) var<storage, read> query: array<f32>;
@group(0) @binding(3) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(256)
fn main(@builtin(workgroup_id) wg: vec3<u32>, @builtin(num_workgroups) nwg: vec3<u32>,
        @builtin(local_invocation_id) lid: vec3<u32>) {
    let i = idx(wg, nwg, lid);
    if (i >= p.n) {
        return;
    }
    let d = p.arg;
    var s = 0.0;
    if (p.slot == 0u) {
        for (var k = 0u; k < d; k = k + 1u) {
            s = s + rows[k * p.n + i] * query[k];
        }
    } else {
        for (var k = 0u; k < d; k = k + 1u) {
            let x = rows[k * p.n + i] - query[k];
            s = s + x * x;
        }
    }
    out[i] = s;
}
";

pub(crate) fn vectors() -> String {
    format!("{COMMON}{VECTORS}")
}

pub(crate) fn vector_patch() -> String {
    format!("{COMMON}{VECTOR_PATCH}")
}

pub(crate) fn propagate() -> String {
    format!("{COMMON}{PROPAGATE}")
}

pub(crate) fn mask() -> String {
    format!("{COMMON}{MASK}")
}

pub(crate) fn bfs() -> String {
    format!("{COMMON}{BFS}")
}

pub(crate) fn wcc() -> String {
    format!("{COMMON}{WCC}")
}

pub(crate) fn triangles() -> String {
    format!("{COMMON}{TRIANGLES}")
}

pub(crate) fn page_rank() -> String {
    format!("{COMMON}{PAGE_RANK}")
}
