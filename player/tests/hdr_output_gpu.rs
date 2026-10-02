//! GPU check of the HDR display output shader (`shader_src::hdr_output`)
//! — the path an EDR display gets instead of the tonemap. Needs no HDR
//! display: the planes are synthetic, the target is an offscreen
//! rgba16float texture read back to the CPU. Skips when no adapter exists.
//!
//! Anchors (all must land on the same PQ code, BT.2408 reference white):
//!   - PQ passthrough of a 203-nit PQ grey is the identity,
//!   - SDR peak white is up-converted to 203 nits,
//!   - HLG 75 % (HLG reference white) maps to ~203 nits at Lw = 1000.

use wgpu::util::DeviceExt;

/// PQ inverse EOTF (fraction of 10 000 nits → signal).
fn pq(nits: f64) -> f64 {
    let (m1, m2, c1, c2, c3) = (0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875);
    let p = (nits / 10000.0).powf(m1);
    ((c1 + c2 * p) / (1.0 + c3 * p)).powf(m2)
}

/// Limited-range code of a normalised luma value, as the shader's
/// `(code * 255 - 16) / 219` expects it, stored in a 16-bit unorm plane.
fn y_code(v: f64) -> u16 {
    (((16.0 + 219.0 * v) / 255.0) * 65535.0).round() as u16
}

const NEUTRAL: u16 = ((128.0 / 255.0) * 65535.0 + 0.5) as u16;

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    // The renderer imports VideoToolbox planes as R16/RG16 unorm too.
    if !adapter.features().contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM) {
        return None;
    }
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_features: wgpu::Features::TEXTURE_FORMAT_16BIT_NORM,
        ..Default::default()
    }))
    .ok()
}

/// Render 4×4 px of uniform planes through `entry`, return the RGB of one texel.
fn render(dev: &wgpu::Device, queue: &wgpu::Queue, entry: &str, y: u16) -> [f32; 3] {
    let plane = |format, data: &[u16], label| {
        dev.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            bytemuck::cast_slice(data),
        )
    };
    let y_tex = plane(wgpu::TextureFormat::R16Unorm, &[y; 16], "y");
    let uv_tex = plane(wgpu::TextureFormat::Rg16Unorm, &[NEUTRAL; 32], "uv");

    let module = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("hdr_output"),
        source: wgpu::ShaderSource::Wgsl(player::shader_src::hdr_output().into()),
    });
    let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let layout = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            tex_entry(0),
            tex_entry(1),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let sampler = dev.create_sampler(&wgpu::SamplerDescriptor::default());
    let bind_group = dev.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&y_tex.create_view(&Default::default())),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&uv_tex.create_view(&Default::default())),
            },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
        ],
    });
    let pl = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let vertex_layout = wgpu::VertexBufferLayout {
        array_stride: 20,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2],
    };
    let pipeline = dev.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(entry),
        layout: Some(&pl),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            buffers: &[vertex_layout],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(entry),
            targets: &[Some(wgpu::TextureFormat::Rgba16Float.into())],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    // Full-target quad, same vertex format as the renderer's.
    #[rustfmt::skip]
    let verts: [f32; 30] = [
        -1.0, -1.0, 0.0, 0.0, 1.0,   1.0, -1.0, 0.0, 1.0, 1.0,   -1.0, 1.0, 0.0, 0.0, 0.0,
        -1.0,  1.0, 0.0, 0.0, 0.0,   1.0, -1.0, 0.0, 1.0, 1.0,    1.0, 1.0, 0.0, 1.0, 0.0,
    ];
    let vb = dev.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let target = dev.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"),
        size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let readback = dev.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 256 * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = dev.create_command_encoder(&Default::default());
    {
        let view = target.create_view(&Default::default());
        let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rp.set_pipeline(&pipeline);
        rp.set_bind_group(0, &bind_group, &[]);
        rp.set_vertex_buffer(0, vb.slice(..));
        rp.draw(0..6, 0..1);
    }
    enc.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(4) },
        },
        wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.unwrap());
    dev.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = slice.get_mapped_range();
    // Texel (1, 1): away from any edge filtering.
    let px: &[u16] = bytemuck::cast_slice(&data[256 + 8..256 + 16]);
    let f = |h: u16| half_to_f32(h);
    [f(px[0]), f(px[1]), f(px[2])]
}

fn half_to_f32(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as u32, (h & 0x3ff) as u32);
    let v = match e {
        0 => (m as f32) * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => f32::from_bits((e + 112) << 23 | m << 13),
    };
    if s == 1 { -v } else { v }
}

fn assert_grey(rgb: [f32; 3], want: f64, tol: f64, what: &str) {
    for c in rgb {
        assert!((c as f64 - want).abs() <= tol, "{what}: got {rgb:?}, want {want:.4} ± {tol}");
    }
}

#[test]
fn hdr_output_shader_lands_on_reference_white() {
    let Some((dev, queue)) = device() else {
        eprintln!("no GPU adapter — skipping");
        return;
    };
    let white = pq(203.0);
    // Tolerances: 16-bit plane quantisation + f16 output (~1e-3 near 0.58).
    assert_grey(render(&dev, &queue, "fs_pq", y_code(white)), white, 2e-3, "PQ passthrough");
    assert_grey(render(&dev, &queue, "fs_sdr", y_code(1.0)), white, 2e-3, "SDR white → 203 nits");
    assert_grey(render(&dev, &queue, "fs_hlg", y_code(0.75)), white, 4e-3, "HLG 75 % → ~203 nits");
    // Black stays black on every path.
    for e in ["fs_pq", "fs_sdr", "fs_hlg"] {
        assert_grey(render(&dev, &queue, e, y_code(0.0)), 0.0, 1e-3, e);
    }
}
