// Frame peak/average signal detection for the HDR → SDR tonemap — WGSL
// port of tonemap.cl detect_peak_avg() (itself ported from libplacebo),
// the dynamic-adaptation half of FFmpeg's tonemap_opencl filter.
//
// The filter folds detection into its tonemap kernel using a
// last-workgroup-finishes trick; a fragment shader can't do workgroup
// reductions, so the same algorithm is split into three compute passes
// dispatched before the HDR draw of every frame:
//
//   cs_publish    (1 thread)      — snapshot the rolling totals into
//                                   out_peak/out_average for the fragment
//                                   shader. Runs FIRST so the published
//                                   stats cover previous frames only,
//                                   exactly like the filter (its kernel
//                                   reads the totals before the last
//                                   workgroup rolls them forward).
//   cs_accumulate (16×16 per wg)  — each invocation reads a 2×2 pixel
//                                   quad (one UV texel), converts to
//                                   destination-space linear RGB with the
//                                   SAME math as the tonemap shader
//                                   (shader_pq_math.wgsl), and adds the
//                                   quad's max signal into a workgroup
//                                   average that feeds the current ring
//                                   slot via atomics.
//   cs_finalize   (1 thread)      — the filter's last-workgroup block:
//                                   normalise the slot, scene-change
//                                   check, roll the 63-frame window.
//
// Composed after shader_pq_math.wgsl and before an input file that
// declares bindings 0/1 and `block_sig` (the quad's max signal):
//   shader_hdr_detect_p010.wgsl — native P010 planes
//   shader_hdr_detect_web.wgsl  — browser RGBA (via shader_chrome_inverse)
//
// Buffer layout mirrors the filter's util_buf (ring of DETECTION_FRAMES+1
// per-frame averages + peaks, running totals, indices) plus the two f32
// result slots the fragment shader reads via group(1) of the tonemap
// shader. wgpu zero-initialises the buffer, which is the filter's
// (implicit) starting state: scene_frame_num == 0 → first frame uses the
// static peak argument and SDR_AVG.

struct TonemapUniforms {
    tone_param: f32,
    desat: f32,
    // Static peak seed in REFERENCE_WHITE units (filter's `peak` arg).
    peak: f32,
    // Scene-change reset threshold (filter's `threshold`, 0 disables).
    scene_threshold: f32,
    // Workgroup count of the cs_accumulate dispatch — the filter's
    // get_num_groups(0) * get_num_groups(1).
    num_wg: u32,
    // Visible content size in pixels; the imported decoder texture may be
    // padded past this, and padding must not skew the statistics.
    frame_w: u32,
    frame_h: u32,
    // Browser variant only (see shader_chrome_inverse.wgsl). 0 on native.
    web_inverse: u32,
}

const DETECTION_FRAMES: u32 = 63u;

struct DetectionBuf {
    avg_buf: array<atomic<u32>, 64>,   // DETECTION_FRAMES + 1
    peak_buf: array<atomic<u32>, 64>,  // DETECTION_FRAMES + 1
    max_total: atomic<u32>,
    avg_total: atomic<u32>,
    frame_idx: atomic<u32>,
    scene_frame_num: atomic<u32>,
    out_peak: f32,
    out_average: f32,
}

@group(0) @binding(2) var<uniform> u_tm: TonemapUniforms;
@group(0) @binding(3) var<storage, read_write> d: DetectionBuf;

@compute @workgroup_size(1)
fn cs_publish() {
    // detect_peak_avg's result selection: static seed until at least one
    // frame of statistics exists, then the rolling window's averages with
    // the filter's floors (peak ≥ 1.0, average ≥ 0.25).
    let n = atomicLoad(&d.scene_frame_num);
    var pk = u_tm.peak;
    var avg = SDR_AVG;
    if (n > 0u) {
        pk = max(1.0, f32(atomicLoad(&d.max_total)) / (REFERENCE_WHITE * f32(n)));
        avg = max(0.25, f32(atomicLoad(&d.avg_total)) / (REFERENCE_WHITE * f32(n)));
    }
    d.out_peak = pk;
    d.out_average = avg;
}

var<workgroup> sum_wg: atomic<u32>;

@compute @workgroup_size(16, 16)
fn cs_accumulate(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) lidx: u32,
) {
    if (lidx == 0u) {
        atomicStore(&sum_wg, 0u);
    }
    workgroupBarrier();

    // One invocation per UV texel = one 2×2 Y quad, like the filter's
    // work items. Edge invocations of the aligned grid clamp into the
    // frame and still contribute — matching its statistics exactly.
    let signal = block_sig(gid.xy);

    // u32() saturates negatives to 0 — out-of-gamut all-negative pixels
    // contribute nothing, like the filter in practice.
    atomicAdd(&sum_wg, u32(signal * REFERENCE_WHITE));
    workgroupBarrier();

    if (lidx == 0u) {
        let avg_wg = atomicLoad(&sum_wg) / 256u; // 16×16 invocations
        let idx = atomicLoad(&d.frame_idx);
        atomicMax(&d.peak_buf[idx], avg_wg);
        atomicAdd(&d.avg_buf[idx], avg_wg);
    }
}

@compute @workgroup_size(1)
fn cs_finalize() {
    let idx = atomicLoad(&d.frame_idx);
    let num_wg = max(u_tm.num_wg, 1u);

    // avg_buf[frame_idx] /= num_wg — finish the frame-average.
    let cur_avg = atomicLoad(&d.avg_buf[idx]) / num_wg;
    atomicStore(&d.avg_buf[idx], cur_avg);

    let n = atomicLoad(&d.scene_frame_num);
    if (u_tm.scene_threshold > 0.0) {
        let cur_max = atomicLoad(&d.peak_buf[idx]);
        let diff = i32(n * cur_avg) - i32(atomicLoad(&d.avg_total));
        if (abs(diff) > i32(f32(n) * u_tm.scene_threshold * REFERENCE_WHITE)) {
            // Scene change: drop the window, keep the current frame as
            // the new scene's first sample.
            for (var i = 0u; i < DETECTION_FRAMES + 1u; i = i + 1u) {
                atomicStore(&d.avg_buf[i], 0u);
            }
            for (var i = 0u; i < DETECTION_FRAMES + 1u; i = i + 1u) {
                atomicStore(&d.peak_buf[i], 0u);
            }
            atomicStore(&d.avg_total, 0u);
            atomicStore(&d.max_total, 0u);
            atomicStore(&d.scene_frame_num, 0u);
            atomicStore(&d.avg_buf[idx], cur_avg);
            atomicStore(&d.peak_buf[idx], cur_max);
        }
    }

    // Add the current frame to the totals, evict the slot it's about to
    // recycle (u32 arithmetic wraps like the filter's unsigned C math).
    let next = (idx + 1u) % (DETECTION_FRAMES + 1u);
    atomicStore(
        &d.max_total,
        atomicLoad(&d.max_total) + atomicLoad(&d.peak_buf[idx]) - atomicLoad(&d.peak_buf[next]),
    );
    atomicStore(
        &d.avg_total,
        atomicLoad(&d.avg_total) + atomicLoad(&d.avg_buf[idx]) - atomicLoad(&d.avg_buf[next]),
    );
    atomicStore(&d.peak_buf[next], 0u);
    atomicStore(&d.avg_buf[next], 0u);
    atomicStore(&d.frame_idx, next);
    atomicStore(&d.scene_frame_num, min(atomicLoad(&d.scene_frame_num) + 1u, DETECTION_FRAMES));
}
