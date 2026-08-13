//! N2 — native wgpu HDR render pass + bloom. The flow particles are drawn as
//! additive HDR points into an off-screen `Rgba16Float` scene target, a bright
//! pass extracts the cores above threshold, a separable gaussian blurs them at
//! half resolution (two iterations), and a final composite tonemaps
//! `scene + bloom` additively onto the egui surface. This is what makes the
//! vortex cores actually *glow* like the `3d_volumetric_analysis` mockup —
//! real Metal/Vulkan/DX12 via wgpu, not a CPU fake.
use eframe::egui_wgpu::{self, CallbackResources, CallbackTrait, ScreenDescriptor};
use eframe::wgpu;
use egui::Rect;

const HDR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// One glowing point, projected CPU-side (see `viewport.rs`).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuInstance {
    pub pos: [f32; 2],   // clip-space NDC within the viewport (y up)
    pub radius_px: f32,  // physical-pixel radius
    pub weight: f32,     // additive coverage 0..~1
    pub color: [f32; 4], // HDR rgb (cores exceed 1.0 so they bloom); a unused
}

/// One glowing streamline segment (a ribbon between two projected points).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SegInstance {
    pub p0: [f32; 2], // NDC endpoints
    pub p1: [f32; 2],
    pub width_px: f32, // ribbon half-width in physical px
    pub _pad: f32,
    pub color: [f32; 4], // HDR rgb
}

const ADDITIVE: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
};

/// Off-screen targets + bind groups, rebuilt whenever the viewport resizes.
struct Targets {
    size: [u32; 2],
    scene: wgpu::TextureView,
    bloom_a: wgpu::TextureView,
    bloom_b: wgpu::TextureView,
    bright_bg: wgpu::BindGroup,    // scene  -> bloom_a
    blur_h_bg: wgpu::BindGroup,    // bloom_a (horizontal)
    blur_v_bg: wgpu::BindGroup,    // bloom_b (vertical)
    composite_bg: wgpu::BindGroup, // scene + bloom_a -> surface
}

/// Long-lived GPU resources, created once and stashed in `callback_resources`.
pub struct BloomRenderer {
    particle_pipe: wgpu::RenderPipeline,
    bright_pipe: wgpu::RenderPipeline,
    blur_pipe: wgpu::RenderPipeline,
    composite_pipe: wgpu::RenderPipeline,
    post_bgl: wgpu::BindGroupLayout,
    comp_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,

    particle_bg: wgpu::BindGroup,
    particle_uni: wgpu::Buffer,
    bright_uni: wgpu::Buffer,
    blur_h_uni: wgpu::Buffer,
    blur_v_uni: wgpu::Buffer,
    comp_uni: wgpu::Buffer,

    instances: wgpu::Buffer,
    instance_cap: u64,
    seg_pipe: wgpu::RenderPipeline,
    segments: wgpu::Buffer,
    seg_cap: u64,
    targets: Option<Targets>,

    // volume raymarch (isosurface / density view)
    volume_pipe: wgpu::RenderPipeline,
    volume_bgl: wgpu::BindGroupLayout,
    volume_uni: wgpu::Buffer,
    vol_tex: Option<(wgpu::Texture, [u32; 3])>,
    vol_view: Option<wgpu::TextureView>,
    volume_bg: Option<wgpu::BindGroup>,
    vol_version: u64,
    // CAD surface-load layer: solid mask + normalized pressure (R8, 3D)
    surf_tex: Option<(wgpu::Texture, wgpu::Texture, [u32; 3])>,
    surf_views: Option<(wgpu::TextureView, wgpu::TextureView)>,
    surf_mask_version: u64,
    surf_pressure_version: u64,
    dummy_views: (wgpu::TextureView, wgpu::TextureView), // keep the layout bound without CAD
}

impl BloomRenderer {
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let particle_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("reyn.particle"),
            source: wgpu::ShaderSource::Wgsl(PARTICLE_WGSL.into()),
        });
        let post_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("reyn.post"),
            source: wgpu::ShaderSource::Wgsl(POST_WGSL.into()),
        });
        let comp_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("reyn.composite"),
            source: wgpu::ShaderSource::Wgsl(COMPOSITE_WGSL.into()),
        });

        // -- bind group layouts ------------------------------------------------
        let uni = |binding, vis| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: vis,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let tex = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let samp = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };

        let particle_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("reyn.particle.bgl"),
            entries: &[uni(0, wgpu::ShaderStages::VERTEX)],
        });
        let post_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("reyn.post.bgl"),
            entries: &[samp(0), tex(1), uni(2, wgpu::ShaderStages::FRAGMENT)],
        });
        let comp_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("reyn.comp.bgl"),
            entries: &[
                samp(0),
                tex(1),
                tex(2),
                uni(3, wgpu::ShaderStages::FRAGMENT),
            ],
        });

        let particle_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("reyn.particle.pl"),
            bind_group_layouts: &[Some(&particle_bgl)],
            immediate_size: 0,
        });
        let post_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("reyn.post.pl"),
            bind_group_layouts: &[Some(&post_bgl)],
            immediate_size: 0,
        });
        let comp_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("reyn.comp.pl"),
            bind_group_layouts: &[Some(&comp_bgl)],
            immediate_size: 0,
        });

        let no_msaa = wgpu::MultisampleState::default(); // count: 1
        let tri = wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        };

        // -- particle pipeline (HDR additive points) ---------------------------
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2, 1 => Float32, 2 => Float32, 3 => Float32x4],
        };
        let particle_pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("reyn.particle.pipe"),
            layout: Some(&particle_pl),
            vertex: wgpu::VertexState {
                module: &particle_mod,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[instance_layout],
            },
            fragment: Some(wgpu::FragmentState {
                module: &particle_mod,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: HDR,
                    blend: Some(ADDITIVE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: tri,
            depth_stencil: None,
            multisample: no_msaa,
            multiview_mask: None,
            cache: None,
        });

        let fs_pipe = |label, pl: &wgpu::PipelineLayout, module, entry, fmt, blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(pl),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: fmt,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: tri,
                depth_stencil: None,
                multisample: no_msaa,
                multiview_mask: None,
                cache: None,
            })
        };
        let bright_pipe = fs_pipe("reyn.bright", &post_pl, &post_mod, "fs_bright", HDR, None);
        let blur_pipe = fs_pipe("reyn.blur", &post_pl, &post_mod, "fs_blur", HDR, None);
        let composite_pipe = fs_pipe(
            "reyn.composite",
            &comp_pl,
            &comp_mod,
            "fs_composite",
            surface_format,
            Some(ADDITIVE),
        );

        // -- volume raymarch pipeline (3D |ω| texture -> HDR scene) -------------
        let volume_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("reyn.volume"),
            source: wgpu::ShaderSource::Wgsl(VOLUME_WGSL.into()),
        });
        let tex3d = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D3,
                multisampled: false,
            },
            count: None,
        };
        let volume_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("reyn.volume.bgl"),
            entries: &[
                uni(0, wgpu::ShaderStages::FRAGMENT),
                tex3d(1),
                samp(2),
                tex3d(3),
                tex3d(4),
            ],
        });
        let volume_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("reyn.volume.pl"),
            bind_group_layouts: &[Some(&volume_bgl)],
            immediate_size: 0,
        });
        let volume_pipe = fs_pipe(
            "reyn.volume",
            &volume_pl,
            &volume_mod,
            "fs_volume",
            HDR,
            None,
        );

        // -- streamline ribbon pipeline (HDR additive tubes, shares particle_uni) --
        let seg_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("reyn.seg"),
            source: wgpu::ShaderSource::Wgsl(SEG_WGSL.into()),
        });
        let seg_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<SegInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2, 1 => Float32x2, 2 => Float32, 3 => Float32, 4 => Float32x4],
        };
        let seg_pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("reyn.seg.pipe"),
            layout: Some(&particle_pl),
            vertex: wgpu::VertexState {
                module: &seg_mod,
                entry_point: Some("vs_seg"),
                compilation_options: Default::default(),
                buffers: &[seg_layout],
            },
            fragment: Some(wgpu::FragmentState {
                module: &seg_mod,
                entry_point: Some("fs_seg"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: HDR,
                    blend: Some(ADDITIVE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: tri,
            depth_stencil: None,
            multisample: no_msaa,
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("reyn.sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uni_buf = |label, size| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let particle_uni = uni_buf("reyn.particle.uni", 16);
        let bright_uni = uni_buf("reyn.bright.uni", 16);
        let blur_h_uni = uni_buf("reyn.blur_h.uni", 16);
        let blur_v_uni = uni_buf("reyn.blur_v.uni", 16);
        let comp_uni = uni_buf("reyn.comp.uni", 16);
        let volume_uni = uni_buf("reyn.volume.uni", 64);

        let dummy3d = |label: &str| {
            let t = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            t.create_view(&Default::default())
        };
        let dummy_views = (dummy3d("reyn.dummy.mask"), dummy3d("reyn.dummy.p"));

        let particle_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("reyn.particle.bg"),
            layout: &particle_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: particle_uni.as_entire_binding(),
            }],
        });

        let instance_cap = 8192;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("reyn.instances"),
            size: instance_cap * std::mem::size_of::<GpuInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let seg_cap = 2048;
        let segments = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("reyn.segments"),
            size: seg_cap * std::mem::size_of::<SegInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            particle_pipe,
            bright_pipe,
            blur_pipe,
            composite_pipe,
            post_bgl,
            comp_bgl,
            sampler,
            particle_bg,
            particle_uni,
            bright_uni,
            blur_h_uni,
            blur_v_uni,
            comp_uni,
            instances,
            instance_cap,
            seg_pipe,
            segments,
            seg_cap,
            targets: None,
            volume_pipe,
            volume_bgl,
            volume_uni,
            vol_tex: None,
            vol_view: None,
            volume_bg: None,
            vol_version: 0,
            surf_tex: None,
            surf_views: None,
            surf_mask_version: 0,
            surf_pressure_version: 0,
            dummy_views,
        }
    }

    fn make_target(&self, device: &wgpu::Device, size: [u32; 2]) -> Targets {
        let half = [size[0].div_ceil(2).max(1), size[1].div_ceil(2).max(1)];
        let target = |w: u32, h: u32, label| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: HDR,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let scene = target(size[0], size[1], "reyn.scene");
        let bloom_a = target(half[0], half[1], "reyn.bloom_a");
        let bloom_b = target(half[0], half[1], "reyn.bloom_b");

        let post_bg = |tex: &wgpu::TextureView, uni: &wgpu::Buffer, label| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.post_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(tex),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: uni.as_entire_binding(),
                    },
                ],
            })
        };
        let bright_bg = post_bg(&scene, &self.bright_uni, "reyn.bright.bg");
        let blur_h_bg = post_bg(&bloom_a, &self.blur_h_uni, "reyn.blur_h.bg");
        let blur_v_bg = post_bg(&bloom_b, &self.blur_v_uni, "reyn.blur_v.bg");
        let composite_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("reyn.composite.bg"),
            layout: &self.comp_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&scene),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&bloom_a),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.comp_uni.as_entire_binding(),
                },
            ],
        });
        Targets {
            size,
            scene,
            bloom_a,
            bloom_b,
            bright_bg,
            blur_h_bg,
            blur_v_bg,
            composite_bg,
        }
    }

    fn ensure_targets(&mut self, device: &wgpu::Device, size: [u32; 2]) {
        if self.targets.as_ref().map(|t| t.size) != Some(size) {
            self.targets = Some(self.make_target(device, size));
        }
    }

    /// Shared bloom-post uniforms (bright threshold, blur dirs+texel, composite params).
    fn write_post_uniforms(
        &self,
        queue: &wgpu::Queue,
        size: [u32; 2],
        strength: f32,
        exposure: f32,
        threshold: f32,
    ) {
        let half = [size[0].div_ceil(2).max(1), size[1].div_ceil(2).max(1)];
        let htex = [1.0 / half[0] as f32, 1.0 / half[1] as f32];
        queue.write_buffer(
            &self.bright_uni,
            0,
            bytemuck::cast_slice(&[threshold, 0.0, 0.0, 0.0]),
        );
        queue.write_buffer(
            &self.blur_h_uni,
            0,
            bytemuck::cast_slice(&[1.0, 0.0, htex[0], htex[1]]),
        );
        queue.write_buffer(
            &self.blur_v_uni,
            0,
            bytemuck::cast_slice(&[0.0, 1.0, htex[0], htex[1]]),
        );
        queue.write_buffer(
            &self.comp_uni,
            0,
            bytemuck::cast_slice(&[strength, exposure, 0.0, 0.0]),
        );
    }

    /// Bright pass + two separable gaussian iterations (scene -> bloom_a).
    fn bloom_post(&self, encoder: &mut wgpu::CommandEncoder) {
        let t = self.targets.as_ref().unwrap();
        fullscreen(
            encoder,
            "reyn.pass.bright",
            &t.bloom_a,
            &self.bright_pipe,
            &t.bright_bg,
        );
        for _ in 0..2 {
            fullscreen(
                encoder,
                "reyn.pass.blurH",
                &t.bloom_b,
                &self.blur_pipe,
                &t.blur_h_bg,
            );
            fullscreen(
                encoder,
                "reyn.pass.blurV",
                &t.bloom_a,
                &self.blur_pipe,
                &t.blur_v_bg,
            );
        }
    }

    /// (Re)upload the 3D scalar (|ω|) texture. Views live for the texture's
    /// lifetime; the caller rebuilds the combined bind group once after all
    /// changed volume/surface resources have been installed.
    fn upload_volume(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &[u8],
        dims: [u32; 3],
    ) {
        if self.vol_tex.as_ref().map(|(_, d)| *d) != Some(dims) {
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("reyn.volume.tex"),
                size: wgpu::Extent3d {
                    width: dims[0],
                    height: dims[1],
                    depth_or_array_layers: dims[2],
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.vol_view = Some(tex.create_view(&Default::default()));
            self.vol_tex = Some((tex, dims));
        }
        let (tex, _) = self.vol_tex.as_ref().unwrap();
        queue.write_texture(
            tex.as_image_copy(),
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(dims[0]),
                rows_per_image: Some(dims[1]),
            },
            wgpu::Extent3d {
                width: dims[0],
                height: dims[1],
                depth_or_array_layers: dims[2],
            },
        );
    }

    /// (Re)upload the CAD surface layer: the solid mask and the normalized
    /// pressure, both R8 3D textures sampled by the raymarch for load shading.
    fn upload_surface(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        mask: &[u8],
        pressure: &[u8],
        dims: [u32; 3],
        upload: (bool, bool),
    ) {
        let (upload_mask, upload_pressure) = upload;
        let recreated = self.surf_tex.as_ref().map(|(_, _, d)| *d) != Some(dims);
        if recreated {
            let mk = |label: &str| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: dims[0],
                        height: dims[1],
                        depth_or_array_layers: dims[2],
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D3,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let mask_texture = mk("reyn.surf.mask");
            let pressure_texture = mk("reyn.surf.p");
            self.surf_views = Some((
                mask_texture.create_view(&Default::default()),
                pressure_texture.create_view(&Default::default()),
            ));
            self.surf_tex = Some((mask_texture, pressure_texture, dims));
        }
        let (mt, pt, _) = self.surf_tex.as_ref().unwrap();
        for (tex, bytes, changed) in [
            (mt, mask, upload_mask || recreated),
            (pt, pressure, upload_pressure || recreated),
        ] {
            if !changed {
                continue;
            }
            queue.write_texture(
                tex.as_image_copy(),
                bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(dims[0]),
                    rows_per_image: Some(dims[1]),
                },
                wgpu::Extent3d {
                    width: dims[0],
                    height: dims[1],
                    depth_or_array_layers: dims[2],
                },
            );
        }
    }

    fn rebuild_volume_bg(&mut self, device: &wgpu::Device) {
        let Some(vol) = self.vol_view.as_ref() else {
            return;
        };
        let (mv, pv) = match self.surf_views.as_ref() {
            Some((m, p)) => (m, p),
            None => (&self.dummy_views.0, &self.dummy_views.1),
        };
        self.volume_bg = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("reyn.volume.bg"),
            layout: &self.volume_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.volume_uni.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(vol),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(mv),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(pv),
                },
            ],
        }));
    }
}

/// Per-frame callback carrying the projected instances + streamline segments.
pub struct FlowCallback {
    pub instances: Vec<GpuInstance>,
    pub segments: Vec<SegInstance>,
    pub size_px: [u32; 2],
    pub bloom_strength: f32,
    pub exposure: f32,
    pub threshold: f32,
}

impl CallbackTrait for FlowCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = resources.get_mut::<BloomRenderer>() else {
            return Vec::new();
        };
        let size = [self.size_px[0].max(1), self.size_px[1].max(1)];
        r.ensure_targets(device, size);

        // grow the instance buffer if the field got denser
        let n = self.instances.len() as u64;
        if n > r.instance_cap {
            r.instance_cap = (n + n / 2).next_power_of_two();
            r.instances = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("reyn.instances"),
                size: r.instance_cap * std::mem::size_of::<GpuInstance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if n > 0 {
            queue.write_buffer(&r.instances, 0, bytemuck::cast_slice(&self.instances));
        }
        // grow + upload streamline segments
        let ns = self.segments.len() as u64;
        if ns > r.seg_cap {
            r.seg_cap = (ns + ns / 2).next_power_of_two();
            r.segments = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("reyn.segments"),
                size: r.seg_cap * std::mem::size_of::<SegInstance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if ns > 0 {
            queue.write_buffer(&r.segments, 0, bytemuck::cast_slice(&self.segments));
        }
        let inv = [1.0 / size[0] as f32, 1.0 / size[1] as f32];
        queue.write_buffer(
            &r.particle_uni,
            0,
            bytemuck::cast_slice(&[inv[0], inv[1], 0.0, 0.0]),
        );
        r.write_post_uniforms(
            queue,
            size,
            self.bloom_strength,
            self.exposure,
            self.threshold,
        );

        // pass 1: additive particles + streamline ribbons -> scene
        {
            let t = r.targets.as_ref().unwrap();
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("reyn.pass.scene"),
                color_attachments: &[Some(color_attach(&t.scene, wgpu::Color::TRANSPARENT))],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if n > 0 {
                rp.set_pipeline(&r.particle_pipe);
                rp.set_bind_group(0, &r.particle_bg, &[]);
                rp.set_vertex_buffer(0, r.instances.slice(..));
                rp.draw(0..6, 0..self.instances.len() as u32);
            }
            if ns > 0 {
                rp.set_pipeline(&r.seg_pipe);
                rp.set_bind_group(0, &r.particle_bg, &[]);
                rp.set_vertex_buffer(0, r.segments.slice(..));
                rp.draw(0..6, 0..self.segments.len() as u32);
            }
        }
        r.bloom_post(encoder);
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::epaint::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        composite_paint(render_pass, resources);
    }
}

/// The final composite is identical for every scene source (points or volume):
/// egui has already set the viewport to our rect, so the fullscreen triangle's
/// 0..1 UVs map 1:1 onto the viewport-sized targets; tonemap `scene + bloom`,
/// blend additively over the panel.
fn composite_paint(render_pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
    let Some(r) = resources.get::<BloomRenderer>() else {
        return;
    };
    let Some(t) = r.targets.as_ref() else { return };
    render_pass.set_pipeline(&r.composite_pipe);
    render_pass.set_bind_group(0, &t.composite_bg, &[]);
    render_pass.draw(0..3, 0..1);
}

/// A refreshable 3D scalar field (normalized |ω|) for the volume raymarch.
#[derive(Clone)]
pub struct VolumeData {
    pub data: std::sync::Arc<Vec<u8>>,
    pub dims: [u32; 3],
    pub version: u64,
}

/// CAD surface-load layer for the raymarch: solid mask + pressure normalized to
/// `[0,1]` (0.5 = zero), both `[dims]` R8 volumes.
#[derive(Clone)]
pub struct SurfaceData {
    pub mask: std::sync::Arc<Vec<u8>>,
    pub pressure: std::sync::Arc<Vec<u8>>,
    pub dims: [u32; 3],
    /// Geometry identity. Horizon/result changes retain this version when the
    /// voxel mask is unchanged, so the mask texture is not re-uploaded.
    pub mask_version: u64,
    /// Display-normalized recovered-pressure identity.
    pub pressure_version: u64,
}

fn surface_upload_plan(
    current_mask_version: u64,
    current_pressure_version: u64,
    views_ready: bool,
    next: &SurfaceData,
) -> (bool, bool) {
    (
        !views_ready || current_mask_version != next.mask_version,
        !views_ready || current_pressure_version != next.pressure_version,
    )
}

/// Per-frame volume raymarch (isosurface / density view). Casts a ray per pixel
/// through the |ω| volume, emission-absorption composites the density window,
/// clips at the slice planes, optionally light-marches for volumetric shadows,
/// and feeds the same bloom post so the isosurfaces glow. With a `surface`
/// layer, rays that enter the solid shade it by display-normalized recovered
/// pressure (red = maximum, blue = minimum), lambert-lit from the mask gradient
/// normal.
#[derive(Clone)]
pub struct VolumeCallback {
    pub size_px: [u32; 2],
    pub eye: [f32; 3],
    /// Look-at target in domain coordinates. Panning moves this away from the
    /// origin, so the shader cannot assume the camera stares at the center.
    pub target: [f32; 3],
    pub tan_half_fov: f32,
    pub density_lo: f32,
    pub density_hi: f32,
    pub slice: [f32; 3], // per-axis clip coord in [-1,1], or -2.0 = plane off
    pub shadows: bool,
    pub bloom_strength: f32,
    pub exposure: f32,
    pub threshold: f32,
    pub volume: Option<VolumeData>,
    pub surface: Option<SurfaceData>,
    /// Model streamline tubes drawn into the same HDR scene after the raymarch.
    /// A separate `add_flow` callback would Clear the scene and wipe the body.
    pub segments: Vec<SegInstance>,
}

impl CallbackTrait for VolumeCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = resources.get_mut::<BloomRenderer>() else {
            return Vec::new();
        };
        let size = [self.size_px[0].max(1), self.size_px[1].max(1)];
        r.ensure_targets(device, size);

        let mut rebuild_bindings = r.volume_bg.is_none();
        if let Some(v) = &self.volume {
            if r.vol_version != v.version || r.volume_bg.is_none() {
                r.upload_volume(device, queue, &v.data, v.dims);
                r.vol_version = v.version;
                rebuild_bindings = true;
            }
        }
        if let Some(s) = &self.surface {
            let (upload_mask, upload_pressure) = surface_upload_plan(
                r.surf_mask_version,
                r.surf_pressure_version,
                r.surf_views.is_some(),
                s,
            );
            if upload_mask || upload_pressure {
                r.upload_surface(
                    device,
                    queue,
                    &s.mask,
                    &s.pressure,
                    s.dims,
                    (upload_mask, upload_pressure),
                );
                r.surf_mask_version = s.mask_version;
                r.surf_pressure_version = s.pressure_version;
                rebuild_bindings = true;
            }
        }
        if rebuild_bindings {
            r.rebuild_volume_bg(device);
        }
        let aspect = size[0] as f32 / size[1] as f32;
        let shadow = if self.shadows { 1.0 } else { 0.0 };
        let surface_on = if self.surface.is_some() { 1.0 } else { 0.0 };
        queue.write_buffer(
            &r.volume_uni,
            0,
            bytemuck::cast_slice(&[
                self.eye[0],
                self.eye[1],
                self.eye[2],
                self.tan_half_fov,
                aspect,
                self.density_lo,
                self.density_hi,
                shadow,
                self.slice[0],
                self.slice[1],
                self.slice[2],
                surface_on,
                self.target[0],
                self.target[1],
                self.target[2],
                0.0,
            ]),
        );
        r.write_post_uniforms(
            queue,
            size,
            self.bloom_strength,
            self.exposure,
            self.threshold,
        );

        {
            let t = r.targets.as_ref().unwrap();
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("reyn.pass.volume"),
                color_attachments: &[Some(color_attach(&t.scene, wgpu::Color::TRANSPARENT))],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(bg) = &r.volume_bg {
                rp.set_pipeline(&r.volume_pipe);
                rp.set_bind_group(0, bg, &[]);
                rp.draw(0..3, 0..1);
            }
        }
        // Composite model tubes without clearing the raymarched body/volume.
        let ns = self.segments.len() as u64;
        if ns > 0 {
            if ns > r.seg_cap {
                r.seg_cap = (ns + ns / 2).next_power_of_two();
                r.segments = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("reyn.segments"),
                    size: r.seg_cap * std::mem::size_of::<SegInstance>() as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            queue.write_buffer(&r.segments, 0, bytemuck::cast_slice(&self.segments));
            let inv = [1.0 / size[0] as f32, 1.0 / size[1] as f32];
            queue.write_buffer(
                &r.particle_uni,
                0,
                bytemuck::cast_slice(&[inv[0], inv[1], 0.0, 0.0]),
            );
            let t = r.targets.as_ref().unwrap();
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("reyn.pass.volume_streamlines"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &t.scene,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&r.seg_pipe);
            rp.set_bind_group(0, &r.particle_bg, &[]);
            rp.set_vertex_buffer(0, r.segments.slice(..));
            rp.draw(0..6, 0..self.segments.len() as u32);
        }
        r.bloom_post(encoder);
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::epaint::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        composite_paint(render_pass, resources);
    }
}

fn color_attach(
    view: &wgpu::TextureView,
    clear: wgpu::Color,
) -> wgpu::RenderPassColorAttachment<'_> {
    wgpu::RenderPassColorAttachment {
        view,
        depth_slice: None,
        resolve_target: None,
        ops: wgpu::Operations {
            load: wgpu::LoadOp::Clear(clear),
            store: wgpu::StoreOp::Store,
        },
    }
}

fn fullscreen(
    encoder: &mut wgpu::CommandEncoder,
    label: &str,
    target: &wgpu::TextureView,
    pipe: &wgpu::RenderPipeline,
    bg: &wgpu::BindGroup,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(color_attach(target, wgpu::Color::TRANSPARENT))],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    rp.set_pipeline(pipe);
    rp.set_bind_group(0, bg, &[]);
    rp.draw(0..3, 0..1);
}

/// Register the renderer's GPU resources once, at app startup.
pub fn install(render_state: &egui_wgpu::RenderState) {
    let renderer = BloomRenderer::new(&render_state.device, render_state.target_format);
    render_state
        .renderer
        .write()
        .callback_resources
        .insert(renderer);
}

/// Queue a bloom-lit flow render for `rect` from pre-projected `instances` and
/// streamline `segments`.
pub fn add_flow(
    ui: &egui::Ui,
    rect: Rect,
    instances: Vec<GpuInstance>,
    segments: Vec<SegInstance>,
) {
    let ppp = ui.ctx().pixels_per_point();
    let size_px = [
        (rect.width() * ppp).round().max(1.0) as u32,
        (rect.height() * ppp).round().max(1.0) as u32,
    ];
    let cb = FlowCallback {
        instances,
        segments,
        size_px,
        bloom_strength: 1.6,
        exposure: 1.25,
        threshold: 1.0,
    };
    ui.painter_at(rect)
        .add(egui_wgpu::Callback::new_paint_callback(rect, cb));
}

/// Queue a bloom-lit **volume raymarch** for `rect`. `eye` and `target` are in
/// domain space ([-1,1]³), `slice[a]` a clip coord in [-1,1] (or -2.0 = off).
/// Optional `segments` are composited into the same HDR scene after the
/// raymarch so model streamlines do not wipe the Cp body.
#[allow(clippy::too_many_arguments)]
pub fn add_volume(
    ui: &egui::Ui,
    rect: Rect,
    eye: [f32; 3],
    target: [f32; 3],
    tan_half_fov: f32,
    density_lo: f32,
    density_hi: f32,
    slice: [f32; 3],
    shadows: bool,
    volume: Option<VolumeData>,
    surface: Option<SurfaceData>,
    segments: Vec<SegInstance>,
) {
    let ppp = ui.ctx().pixels_per_point();
    let size_px = [
        (rect.width() * ppp).round().max(1.0) as u32,
        (rect.height() * ppp).round().max(1.0) as u32,
    ];
    let cb = VolumeCallback {
        size_px,
        eye,
        target,
        tan_half_fov,
        density_lo,
        density_hi,
        slice,
        shadows,
        // CAD surface-load shading reads poorly under the sandbox bloom amount.
        bloom_strength: if surface.is_some() { 0.55 } else { 1.5 },
        exposure: if surface.is_some() { 1.05 } else { 1.2 },
        threshold: if surface.is_some() { 1.35 } else { 1.0 },
        volume,
        surface,
        segments,
    };
    ui.painter_at(rect)
        .add(egui_wgpu::Callback::new_paint_callback(rect, cb));
}

// -- shaders -----------------------------------------------------------------

const PARTICLE_WGSL: &str = r#"
struct Uni { inv_viewport: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> U: Uni;

struct Inst {
  @location(0) pos: vec2<f32>,
  @location(1) radius_px: f32,
  @location(2) weight: f32,
  @location(3) color: vec4<f32>,
};
struct VOut {
  @builtin(position) clip: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec3<f32>,
  @location(2) weight: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Inst) -> VOut {
  var corners = array<vec2<f32>, 6>(
    vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
    vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
  let c = corners[vi];
  let offset = c * inst.radius_px * U.inv_viewport * 2.0;
  var o: VOut;
  o.clip = vec4<f32>(inst.pos + offset, 0.0, 1.0);
  o.uv = c;
  o.color = inst.color.rgb;
  o.weight = inst.weight;
  return o;
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
  let r2 = dot(in.uv, in.uv);
  if (r2 > 1.0) { discard; }
  let g = exp(-3.2 * r2);            // soft gaussian dot
  return vec4<f32>(in.color * in.weight * g, 1.0);
}
"#;

const POST_WGSL: &str = r#"
@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var<uniform> P: vec4<f32>; // bright: x=threshold | blur: xy=dir, zw=texel

struct FOut { @builtin(position) clip: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> FOut {
  var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
  let xy = p[vi];
  var o: FOut;
  o.clip = vec4<f32>(xy, 0.0, 1.0);
  o.uv = vec2<f32>((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
  return o;
}

@fragment
fn fs_bright(in: FOut) -> @location(0) vec4<f32> {
  let c = textureSample(tex, samp, in.uv).rgb;
  let l = max(max(c.r, c.g), c.b);
  let k = max(l - P.x, 0.0) / max(l, 1e-4);
  return vec4<f32>(c * k, 1.0);
}

@fragment
fn fs_blur(in: FOut) -> @location(0) vec4<f32> {
  let dir = P.xy;
  let texel = P.zw;
  var w = array<f32, 5>(0.227027, 0.194594, 0.121621, 0.054054, 0.016216);
  var acc = textureSample(tex, samp, in.uv).rgb * w[0];
  for (var i = 1; i < 5; i = i + 1) {
    let off = dir * texel * f32(i);
    acc = acc + textureSample(tex, samp, in.uv + off).rgb * w[i];
    acc = acc + textureSample(tex, samp, in.uv - off).rgb * w[i];
  }
  return vec4<f32>(acc, 1.0);
}
"#;

const COMPOSITE_WGSL: &str = r#"
@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var scene: texture_2d<f32>;
@group(0) @binding(2) var bloom: texture_2d<f32>;
@group(0) @binding(3) var<uniform> C: vec4<f32>; // x=bloom_strength, y=exposure

struct FOut { @builtin(position) clip: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> FOut {
  var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
  let xy = p[vi];
  var o: FOut;
  o.clip = vec4<f32>(xy, 0.0, 1.0);
  o.uv = vec2<f32>((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
  return o;
}

@fragment
fn fs_composite(in: FOut) -> @location(0) vec4<f32> {
  let s = textureSample(scene, samp, in.uv).rgb;
  let b = textureSample(bloom, samp, in.uv).rgb;
  let hdr = s + b * C.x;
  let mapped = vec3<f32>(1.0) - exp(-hdr * C.y);   // filmic-ish tonemap
  return vec4<f32>(mapped, 1.0);
}
"#;

const SEG_WGSL: &str = r#"
struct Uni { inv_viewport: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> U: Uni;

struct Seg {
  @location(0) p0: vec2<f32>,
  @location(1) p1: vec2<f32>,
  @location(2) width_px: f32,
  @location(3) pad: f32,
  @location(4) color: vec4<f32>,
};
struct VOut {
  @builtin(position) clip: vec4<f32>,
  @location(0) edge: f32,
  @location(1) color: vec3<f32>,
};

@vertex
fn vs_seg(@builtin(vertex_index) vi: u32, s: Seg) -> VOut {
  var tv = array<f32, 6>(0.0, 1.0, 1.0, 0.0, 1.0, 0.0); // along the segment
  var ev = array<f32, 6>(-1.0, -1.0, 1.0, -1.0, 1.0, 1.0); // across the ribbon
  let t = tv[vi];
  let e = ev[vi];
  let dir = s.p1 - s.p0;
  let ndir = dir / max(length(dir), 1e-5);
  let perp = vec2<f32>(-ndir.y, ndir.x);
  let base = mix(s.p0, s.p1, t);
  let off = perp * e * s.width_px * U.inv_viewport * 2.0;
  var o: VOut;
  o.clip = vec4<f32>(base + off, 0.0, 1.0);
  o.edge = e;
  o.color = s.color.rgb;
  return o;
}

@fragment
fn fs_seg(in: VOut) -> @location(0) vec4<f32> {
  let g = exp(-2.5 * in.edge * in.edge); // round tube cross-section
  return vec4<f32>(in.color * g, 1.0);
}
"#;

const VOLUME_WGSL: &str = r#"
struct VU {
  eye_fov: vec4<f32>,   // xyz = eye (domain space), w = tan(half fov)
  params:  vec4<f32>,   // aspect, density_lo, density_hi, shadows
  slice:   vec4<f32>,   // xyz = per-axis clip coord in [-1,1] (-2 = off), w = surface layer on
  // `target` is a WGSL reserved keyword, hence the spelled-out name.
  look_at: vec4<f32>,   // xyz = look-at point (domain space), w unused
};
@group(0) @binding(0) var<uniform> V: VU;
@group(0) @binding(1) var vol: texture_3d<f32>;
@group(0) @binding(2) var vs: sampler;
@group(0) @binding(3) var solid: texture_3d<f32>;   // CAD mask
@group(0) @binding(4) var press: texture_3d<f32>;   // normalized pressure (0.5 = 0)

struct FOut { @builtin(position) clip: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> FOut {
  var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
  let xy = p[vi];
  var o: FOut;
  o.clip = vec4<f32>(xy, 0.0, 1.0);
  o.uv = vec2<f32>((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
  return o;
}

fn dens(uvw: vec3<f32>) -> f32 {
  return textureSampleLevel(vol, vs, uvw, 0.0).r;
}

fn solid_at(uvw: vec3<f32>) -> f32 {
  return textureSampleLevel(solid, vs, uvw, 0.0).r;
}

fn ray_clipped(pw: vec3<f32>) -> bool {
  return (V.slice.x > -1.5 && pw.x < V.slice.x) ||
         (V.slice.y > -1.5 && pw.y < V.slice.y) ||
         (V.slice.z > -1.5 && pw.z < V.slice.z);
}

/// Recovered-pressure color map: blue (minimum) ↔ warm body ↔ red (maximum).
/// The neutral base stays bright enough that a zero-Cp shell is still readable
/// on the near-black Results canvas (a pure dark mix vanished into the well).
fn load_color(t: f32) -> vec3<f32> {
  let body = vec3<f32>(0.78, 0.68, 0.58);
  let red = vec3<f32>(0.95, 0.32, 0.24);
  let blue = vec3<f32>(0.28, 0.52, 0.95);
  if (t >= 0.0) { return mix(body, red, clamp(t, 0.0, 1.0)); }
  return mix(body, blue, clamp(-t, 0.0, 1.0));
}

@fragment
fn fs_volume(in: FOut) -> @location(0) vec4<f32> {
  let eye = V.eye_fov.xyz;
  let tanH = V.eye_fov.w;
  let aspect = V.params.x;
  let dlo = V.params.y;
  let dhi = V.params.z;
  let shadows = V.params.w;

  let ndc = vec2<f32>(in.uv.x * 2.0 - 1.0, 1.0 - in.uv.y * 2.0);
  let fwd = normalize(V.look_at.xyz - eye);
  // World up is +Z (the physics vertical axis); straight down the pole the
  // reference swings to +X. The threshold matches viewport::UP_REFERENCE_LIMIT
  // exactly, or CPU annotations would drift off the raymarched image.
  var up_ref = vec3<f32>(0.0, 0.0, 1.0);
  if (abs(fwd.z) > 0.99995) { up_ref = vec3<f32>(1.0, 0.0, 0.0); }
  let right = normalize(cross(fwd, up_ref));
  let up = cross(right, fwd);
  let dir = normalize(fwd + tanH * (ndc.x * aspect * right + ndc.y * up));

  // ray vs domain box [-1,1]^3
  let inv = 1.0 / dir;
  let ta = (vec3<f32>(-1.0) - eye) * inv;
  let tb = (vec3<f32>(1.0) - eye) * inv;
  let tsmall = min(ta, tb);
  let tbig = max(ta, tb);
  let tmin = max(max(tsmall.x, tsmall.y), tsmall.z);
  let tmax = min(min(tbig.x, tbig.y), tbig.z);
  if (tmax <= max(tmin, 0.0)) { discard; }

  let start = max(tmin, 0.0);
  let STEPS = 96;
  let dt = (tmax - start) / f32(STEPS);
  let ldir = normalize(vec3<f32>(0.4, 1.0, 0.35));

  // Find the first solid hit so Q/vorticity fog cannot extinguish the ray
  // before the Cp body is shaded (that read as a translucent blob).
  var t_solid = tmax + 1.0;
  var solid_uvw = vec3<f32>(0.0);
  if (V.slice.w > 0.5) {
    for (var i = 0; i < STEPS; i = i + 1) {
      let t = start + (f32(i) + 0.5) * dt;
      let pw = eye + dir * t;
      if (ray_clipped(pw)) { continue; }
      let uvw = pw * 0.5 + 0.5;
      if (solid_at(uvw) > 0.5) {
        t_solid = t;
        solid_uvw = uvw;
        break;
      }
    }
  }

  var col = vec3<f32>(0.0);
  var trans = 1.0;
  for (var i = 0; i < STEPS; i = i + 1) {
    let t = start + (f32(i) + 0.5) * dt;
    if (t >= t_solid) { break; }
    let pw = eye + dir * t;
    if (ray_clipped(pw)) { continue; }
    let uvw = pw * 0.5 + 0.5;
    let s = dens(uvw);
    let d = smoothstep(dlo, dhi, s);
    if (d > 0.002) {
      let a = d * 0.08;
      var lit = vec3<f32>(0.55 + 1.7 * s, 0.24 + 0.85 * s, 0.05 + 0.12 * s) * (0.5 + 3.2 * d);
      if (shadows > 0.5) {
        var occ = 0.0;
        for (var k = 1; k <= 6; k = k + 1) {
          let lp = uvw + ldir * (f32(k) * 0.06);
          if (all(lp >= vec3<f32>(0.0)) && all(lp <= vec3<f32>(1.0))) {
            occ = occ + smoothstep(dlo, dhi, dens(lp)) * 0.18;
          }
        }
        lit = lit * exp(-occ * 2.2);
      }
      col = col + trans * a * lit;
      trans = trans * (1.0 - a);
    }
  }
  if (t_solid <= tmax) {
    let e = 1.5 / 64.0;
    let nrm = normalize(vec3<f32>(
      solid_at(solid_uvw - vec3<f32>(e, 0.0, 0.0)) - solid_at(solid_uvw + vec3<f32>(e, 0.0, 0.0)),
      solid_at(solid_uvw - vec3<f32>(0.0, e, 0.0)) - solid_at(solid_uvw + vec3<f32>(0.0, e, 0.0)),
      solid_at(solid_uvw - vec3<f32>(0.0, 0.0, e)) - solid_at(solid_uvw + vec3<f32>(0.0, 0.0, e))) + vec3<f32>(1e-5));
    let pv = textureSampleLevel(press, vs, solid_uvw, 0.0).r * 2.0 - 1.0;
    let lambert = 0.48 + 0.62 * max(dot(nrm, ldir), 0.0);
    let rim = pow(1.0 - abs(dot(nrm, dir)), 2.0) * 0.32;
    let surf = load_color(pv) * lambert + vec3<f32>(rim * 0.85);
    col = col + trans * surf;
    trans = 0.0;
  }
  return vec4<f32>(col, 1.0 - trans);
}
"#;

// -- headless GPU tests ------------------------------------------------------
// Run each scene pipeline (points or volume) → bright → blur×4 → composite into
// an off-screen surface texture on the real GPU, read it back, and assert the
// result glows. Compiles every WGSL shader and exercises every pass under wgpu
// validation. Skip gracefully if the CI/sandbox has no adapter. Mirrors the N1
// `engine_round_trip` integration test.
#[cfg(test)]
mod tests {
    use super::*;

    const S: u32 = 64;

    #[test]
    fn surface_upload_plan_skips_unchanged_geometry() {
        let surface = SurfaceData {
            mask: std::sync::Arc::new(vec![255; 8]),
            pressure: std::sync::Arc::new(vec![128; 8]),
            dims: [2; 3],
            mask_version: 4,
            pressure_version: 9,
        };
        assert_eq!(surface_upload_plan(4, 8, true, &surface), (false, true));
        assert_eq!(surface_upload_plan(4, 9, true, &surface), (false, false));
        assert_eq!(surface_upload_plan(4, 9, false, &surface), (true, true));
    }

    fn luma(px: &[u8]) -> u16 {
        px[0].max(px[1]).max(px[2]) as u16
    }

    fn gpu_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::default();
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .ok()?;
        Some(
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("request_device"),
        )
    }

    /// Every shader must parse and validate without a GPU. The render tests
    /// above skip themselves when no adapter is present, so on an adapterless
    /// machine this is the only thing standing between a typo — or a WGSL
    /// reserved word used as a struct field — and a crash on first paint.
    #[test]
    fn every_shader_parses_and_validates() {
        for (label, source) in [
            ("particle", PARTICLE_WGSL),
            ("post", POST_WGSL),
            ("composite", COMPOSITE_WGSL),
            ("segment", SEG_WGSL),
            ("volume", VOLUME_WGSL),
        ] {
            let module = naga::front::wgsl::parse_str(source)
                .unwrap_or_else(|error| panic!("{label} shader does not parse: {error:?}"));
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::default(),
            )
            .validate(&module)
            .unwrap_or_else(|error| panic!("{label} shader does not validate: {error:?}"));
        }
    }

    /// Render one callback into a 64² Rgba8Unorm surface (cleared black) exactly
    /// as egui would (prepare → set viewport → paint), read it back, and assert
    /// no validation errors. Returns the RGBA bytes.
    fn render(device: &wgpu::Device, queue: &wgpu::Queue, cb: impl CallbackTrait) -> Vec<u8> {
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut resources = CallbackResources::default();
        resources.insert(BloomRenderer::new(device, wgpu::TextureFormat::Rgba8Unorm));

        let out = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test.out"),
            size: wgpu::Extent3d {
                width: S,
                height: S,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let out_view = out.create_view(&Default::default());
        let screen = ScreenDescriptor {
            size_in_pixels: [S, S],
            pixels_per_point: 1.0,
        };
        let mut encoder = device.create_command_encoder(&Default::default());
        let extra = cb.prepare(device, queue, &screen, &mut encoder, &mut resources);
        {
            let mut rp = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("test.main"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &out_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            rp.set_viewport(0.0, 0.0, S as f32, S as f32, 0.0, 1.0);
            let full =
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(S as f32, S as f32));
            let info = egui::epaint::PaintCallbackInfo {
                viewport: full,
                clip_rect: full,
                pixels_per_point: 1.0,
                screen_size_px: [S, S],
            };
            cb.paint(info, &mut rp, &resources);
        }

        let bpr = 4 * S; // 256, row-aligned
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test.readback"),
            size: (bpr * S) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            out.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: S,
                height: S,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(extra.into_iter().chain(std::iter::once(encoder.finish())));
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let bytes = buf.slice(..).get_mapped_range().to_vec();
        buf.unmap();
        assert!(
            pollster::block_on(scope.pop()).is_none(),
            "wgpu validation error"
        );
        bytes
    }

    fn at(px: &[u8], x: u32, y: u32) -> u16 {
        let o = (y * 4 * S + x * 4) as usize;
        luma(&px[o..o + 4])
    }

    #[test]
    fn bloom_renders_and_glows() {
        let Some((device, queue)) = gpu_device() else {
            eprintln!("no wgpu adapter — skipping bloom test");
            return;
        };
        let cb = FlowCallback {
            instances: vec![GpuInstance {
                pos: [0.0, 0.0],
                radius_px: 16.0,
                weight: 1.0,
                color: [4.0, 2.6, 0.8, 1.0],
            }],
            // a gold streamline ribbon along the bottom exercises the seg pipeline
            segments: vec![SegInstance {
                p0: [-0.6, -0.6],
                p1: [0.6, -0.6],
                width_px: 3.0,
                _pad: 0.0,
                color: [1.5, 1.0, 0.35, 1.0],
            }],
            size_px: [S, S],
            bloom_strength: 1.6,
            exposure: 1.25,
            threshold: 1.0,
        };
        let px = render(&device, &queue, cb);
        let (center, ring) = (at(&px, 32, 32), at(&px, 32, 11));
        assert!(
            center > 40,
            "core did not render bright (center luma {center})"
        );
        assert!(
            ring > 2,
            "bloom did not spread beyond the core (ring luma {ring})"
        );
        // the ribbon sits in the lower half (NDC y=-0.6 → ~pixel row 51) and glows
        assert!(
            at(&px, 32, 51) > 15,
            "streamline ribbon did not render (luma {})",
            at(&px, 32, 51)
        );
    }

    #[test]
    fn volume_raymarch_glows() {
        let Some((device, queue)) = gpu_device() else {
            eprintln!("no wgpu adapter — skipping volume test");
            return;
        };
        // a dense gaussian blob at the centre of a 16³ |ω| volume
        let n = 16usize;
        let mut data = vec![0u8; n * n * n];
        let c = (n as f32 - 1.0) * 0.5;
        for k in 0..n {
            for j in 0..n {
                for i in 0..n {
                    let d2 =
                        (i as f32 - c).powi(2) + (j as f32 - c).powi(2) + (k as f32 - c).powi(2);
                    data[(k * n + j) * n + i] = ((-d2 / 8.0).exp() * 255.0) as u8;
                }
            }
        }
        let cb = VolumeCallback {
            size_px: [S, S],
            eye: [0.0, 0.0, 4.0], // looks down -z at the domain origin
            target: [0.0; 3],
            tan_half_fov: 0.55,
            density_lo: 0.15,
            density_hi: 0.6,
            slice: [-2.0, -2.0, -2.0],
            shadows: false,
            bloom_strength: 1.5,
            exposure: 1.2,
            threshold: 1.0,
            volume: Some(VolumeData {
                data: std::sync::Arc::new(data.clone()),
                dims: [n as u32; 3],
                version: 1,
            }),
            surface: None,
            segments: Vec::new(),
        };
        let px = render(&device, &queue, cb.clone());
        let (center, corner) = (at(&px, 32, 32), at(&px, 6, 6));
        assert!(
            center > 30,
            "volume core not visible (center luma {center})"
        );
        assert!(
            center > corner + 10,
            "no isosurface contrast (center {center} vs corner {corner})"
        );

        // Panning is a look-at move, so the raymarch has to honour the target
        // the CPU projection uses. Aiming off the blob must slide it out of the
        // middle of frame; the screen direction depends on the camera basis, so
        // the assertion is that the centre empties, not which way it goes.
        let panned = VolumeCallback {
            target: [0.0, 0.6, 0.0],
            ..cb.clone()
        };
        let px = render(&device, &queue, panned);
        let off_axis = at(&px, 32, 32);
        assert!(
            off_axis + 10 < center,
            "look-at target did not move the raymarch (centre {center} → {off_axis})"
        );

        // with a CAD surface layer: a solid block with high frontal pressure must
        // render opaque (the load-map shading path, exercising the new bindings)
        let mut mask = vec![0u8; n * n * n];
        let mut press = vec![128u8; n * n * n]; // 0.5 = zero pressure
        for k in 6..10 {
            for j in 6..10 {
                for i in 6..10 {
                    mask[(k * n + j) * n + i] = 255;
                    press[(k * n + j) * n + i] = 240; // strong +p → red load
                }
            }
        }
        let cb = VolumeCallback {
            size_px: [S, S],
            eye: [0.0, 0.0, 4.0],
            target: [0.0; 3],
            tan_half_fov: 0.55,
            density_lo: 0.15,
            density_hi: 0.6,
            slice: [-2.0, -2.0, -2.0],
            shadows: false,
            bloom_strength: 1.5,
            exposure: 1.2,
            threshold: 1.0,
            volume: Some(VolumeData {
                data: std::sync::Arc::new(data),
                dims: [n as u32; 3],
                version: 2,
            }),
            surface: Some(SurfaceData {
                mask: std::sync::Arc::new(mask),
                pressure: std::sync::Arc::new(press),
                dims: [n as u32; 3],
                mask_version: 1,
                pressure_version: 1,
            }),
            segments: Vec::new(),
        };
        let px = render(&device, &queue, cb);
        let o = ((32u32 * 4 * S) + 32 * 4) as usize;
        let (r, g, b) = (px[o] as i32, px[o + 1] as i32, px[o + 2] as i32);
        assert!(
            r > 40 && r > g && r > b,
            "load surface should shade red-dominant (rgb {r},{g},{b})"
        );
    }

    /// N2-AC1: end-to-end throughput for 1M point-sprites (upload + additive
    /// particle pass + bright + blur×4 + composite) at a realistic 1280×800.
    /// Ignored by default (perf, not correctness):
    ///   cargo test -p reyn-studio -- --ignored --nocapture bench_million_points
    #[test]
    #[ignore = "perf benchmark — run with --ignored --nocapture"]
    fn bench_million_points() {
        let Some((device, queue)) = gpu_device() else {
            eprintln!("no wgpu adapter — skipping benchmark");
            return;
        };
        let n: u32 = 1_000_000;
        let mut st = 0x1234_5678u32;
        let mut rnd = move || {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            st as f32 / u32::MAX as f32
        };
        let instances: Vec<GpuInstance> = (0..n)
            .map(|_| GpuInstance {
                pos: [rnd() * 2.0 - 1.0, rnd() * 2.0 - 1.0],
                radius_px: 2.0,
                weight: 0.4,
                color: [1.3, 0.75, 0.2, 1.0],
            })
            .collect();

        let size = [1280u32, 800u32];
        let mut resources = CallbackResources::default();
        resources.insert(BloomRenderer::new(&device, wgpu::TextureFormat::Rgba8Unorm));
        let cb = FlowCallback {
            instances,
            segments: vec![],
            size_px: size,
            bloom_strength: 1.6,
            exposure: 1.25,
            threshold: 1.0,
        };
        let screen = ScreenDescriptor {
            size_in_pixels: size,
            pixels_per_point: 1.0,
        };
        let out = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bench.out"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let out_view = out.create_view(&Default::default());

        let one_frame = |resources: &mut CallbackResources| {
            let mut encoder = device.create_command_encoder(&Default::default());
            let extra = cb.prepare(&device, &queue, &screen, &mut encoder, resources);
            {
                let mut rp = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("bench.main"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &out_view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    })
                    .forget_lifetime();
                rp.set_viewport(0.0, 0.0, size[0] as f32, size[1] as f32, 0.0, 1.0);
                let full = egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(size[0] as f32, size[1] as f32),
                );
                let info = egui::epaint::PaintCallbackInfo {
                    viewport: full,
                    clip_rect: full,
                    pixels_per_point: 1.0,
                    screen_size_px: size,
                };
                cb.paint(info, &mut rp, resources);
            }
            queue.submit(extra.into_iter().chain(std::iter::once(encoder.finish())));
        };

        one_frame(&mut resources); // warmup (pipeline/target creation)
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();

        let frames = 60;
        let t0 = std::time::Instant::now();
        for _ in 0..frames {
            one_frame(&mut resources);
        }
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let per = t0.elapsed().as_secs_f64() / frames as f64;
        let fps = 1.0 / per;
        println!(
            "\n[bench] {n} points @ {}x{}: {:.2} ms/frame · {:.0} fps · {:.1}M pts/s (upload+render+bloom+composite)\n",
            size[0], size[1], per * 1000.0, fps, n as f64 * fps / 1e6
        );
        assert!(
            fps > 30.0,
            "1M-point pipeline below the 30 fps floor ({fps:.0})"
        );
    }
}
