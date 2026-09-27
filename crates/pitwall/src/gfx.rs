//! wgpu renderer.
//!
//! Three passes per frame:
//!
//! 1. **Backdrop** — a fullscreen triangle into an offscreen target.
//! 2. **Blur** — a dual-Kawase down/up chain over that target.
//! 3. **Composite** — every UI element as one instanced draw, sampling the
//!    blurred result for the glass.
//!
//! # The latency configuration is the point
//!
//! See [`Renderer::configure_surface`]. `desired_maximum_frame_latency = 1`
//! and a non-FIFO present mode are, together, worth more perceived latency
//! than every other optimisation in this repository combined. The default
//! swapchain queues two to three frames; at 60 Hz that is 33-50 ms of pure
//! added delay between the telemetry being decoded and the photons leaving
//! the panel — larger than the sim's own send interval, the network hop, and
//! the decode put together.

use std::sync::Arc;
use wgpu::util::DeviceExt;

use crate::font;
use crate::ui::Instance;

/// How many halvings the blur chain performs. Five levels on a 1080p window
/// takes the effective radius past 100 px while sampling less than one full
/// screen of texels in total.
const BLUR_LEVELS: usize = 5;

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Uniforms {
    pub resolution: [f32; 2],
    pub time: f32,
    pub dpi: f32,

    pub rpm_frac: f32,
    pub throttle: f32,
    pub brake: f32,
    pub steer: f32,

    pub speed_norm: f32,
    pub gear: f32,
    pub lat_g: f32,
    pub long_g: f32,

    pub balance: f32,
    pub connected: f32,
    pub sim_id: f32,
    pub frame_ms: f32,

    pub limiter: f32,
    pub shift_pulse: f32,
    pub slip: f32,
    pub _pad: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurParams {
    texel: [f32; 2],
    offset: f32,
    _pad: f32,
}

struct BlurTarget {
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

/// Where a frame is presented.
///
/// Splitting this out is what makes the renderer testable: the same pipelines,
/// shaders and draw list can target a swapchain or an offscreen texture, so a
/// frame can be rendered and inspected on a machine with no display — which is
/// how the typography and layout are verified rather than asserted.
enum Target {
    Window {
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
    },
    Offscreen {
        texture: wgpu::Texture,
        view: wgpu::TextureView,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    },
}

impl Target {
    fn size(&self) -> (u32, u32) {
        match self {
            Target::Window { config, .. } => (config.width, config.height),
            Target::Offscreen { width, height, .. } => (*width, *height),
        }
    }
    fn format(&self) -> wgpu::TextureFormat {
        match self {
            Target::Window { config, .. } => config.format,
            Target::Offscreen { format, .. } => *format,
        }
    }
}

pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    target: Target,

    uniform_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_capacity: usize,

    backdrop_pipeline: wgpu::RenderPipeline,
    backdrop_bg: wgpu::BindGroup,

    blur_down_pipeline: wgpu::RenderPipeline,
    blur_up_pipeline: wgpu::RenderPipeline,
    blur_bgl: wgpu::BindGroupLayout,

    composite_pipeline: wgpu::RenderPipeline,
    composite_bgl: wgpu::BindGroupLayout,
    composite_bg: Option<wgpu::BindGroup>,

    sampler: wgpu::Sampler,
    font_view: wgpu::TextureView,

    targets: Vec<BlurTarget>,
    down_bgs: Vec<wgpu::BindGroup>,
    up_bgs: Vec<wgpu::BindGroup>,

    /// Set when the surface reports the window is gone; the caller should stop.
    pub surface_lost: bool,
}

impl Renderer {
    /// Windowed renderer: presents to the OS swapchain.
    pub async fn new(window: Arc<winit::window::Window>) -> anyhow_lite::Result<Self> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("create_surface: {e}"))?;

        let adapter = Self::pick_adapter(&instance, Some(&surface)).await?;
        let (device, queue) = Self::open_device(&adapter).await?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        // Present mode, in order of preference. Mailbox gives us the lowest
        // latency without tearing; Immediate is lower still but tears; Fifo
        // is the universally-supported fallback and is what everything else
        // ships by default.
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else {
            wgpu::PresentMode::Fifo
        };
        eprintln!("[gfx] present mode: {present_mode:?}, format: {format:?}");

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            // Auto reproduces wgpu's historical behaviour: sRGB for our
            // 8-bit surface format. We deliberately do not opt into a
            // wide-gamut space — the palette is authored in sRGB.
            color_space: wgpu::SurfaceColorSpace::Auto,
            width,
            height,
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            // *** The single highest-leverage line in the renderer. ***
            // Default is 2-3 queued frames. One means the GPU works on the
            // frame we just submitted rather than one from 33 ms ago.
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        Ok(Self::build(device, queue, Target::Window { surface, config }))
    }

    /// Headless renderer: draws into an offscreen texture that can be read
    /// back with [`Self::read_pixels`].
    ///
    /// No window, no swapchain, no display required. This is what makes the
    /// UI verifiable — a frame can be rendered in CI and compared, instead of
    /// the layout and the typography being taken on trust.
    pub async fn new_headless(width: u32, height: u32) -> anyhow_lite::Result<Self> {
        let width = width.max(1);
        let height = height.max(1);
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = Self::pick_adapter(&instance, None).await?;
        let (device, queue) = Self::open_device(&adapter).await?;

        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("headless-target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());

        Ok(Self::build(
            device,
            queue,
            Target::Offscreen { texture, view, width, height, format },
        ))
    }

    async fn pick_adapter(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'static>>,
    ) -> anyhow_lite::Result<wgpu::Adapter> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: surface,
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })
            .await
            .map_err(|e| format!("no suitable GPU adapter: {e}"))?;
        let info = adapter.get_info();
        eprintln!(
            "[gfx] adapter: {} ({:?}, {:?})",
            info.name, info.device_type, info.backend
        );
        Ok(adapter)
    }

    async fn open_device(
        adapter: &wgpu::Adapter,
    ) -> anyhow_lite::Result<(wgpu::Device, wgpu::Queue)> {
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("pitwall-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|e| format!("request_device: {e}").into())
    }

    /// Shared pipeline and resource construction, identical for both targets.
    fn build(device: wgpu::Device, queue: wgpu::Queue, target: Target) -> Self {
        let fmt = target.format();

        // ---- shared resources ------------------------------------------------
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let instance_capacity = 2048;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-instances"),
            size: (instance_capacity * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear-clamp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let font_view = Self::upload_font(&device, &queue);

        // ---- backdrop --------------------------------------------------------
        let backdrop_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("backdrop"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/backdrop.wgsl").into()),
        });
        let backdrop_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("backdrop-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let backdrop_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("backdrop-bg"),
            layout: &backdrop_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });
        let backdrop_pipeline = Self::fullscreen_pipeline(
            &device,
            &backdrop_shader,
            "vs_main",
            "fs_main",
            &backdrop_bgl,
            wgpu::TextureFormat::Rgba16Float,
            "backdrop",
        );

        // ---- blur ------------------------------------------------------------
        let blur_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blur"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/blur.wgsl").into()),
        });
        let blur_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blur-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let blur_down_pipeline = Self::fullscreen_pipeline(
            &device,
            &blur_shader,
            "vs_main",
            "fs_down",
            &blur_bgl,
            wgpu::TextureFormat::Rgba16Float,
            "blur-down",
        );
        let blur_up_pipeline = Self::fullscreen_pipeline(
            &device,
            &blur_shader,
            "vs_main",
            "fs_up",
            &blur_bgl,
            wgpu::TextureFormat::Rgba16Float,
            "blur-up",
        );

        // ---- composite -------------------------------------------------------
        let composite_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/composite.wgsl").into()),
        });
        let composite_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let composite_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite-layout"),
            bind_group_layouts: &[Some(&composite_bgl)],
            immediate_size: 0,
        });

        let composite_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("composite"),
                layout: Some(&composite_layout),
                vertex: wgpu::VertexState {
                    module: &composite_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Instance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 0,
                                shader_location: 0,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 16,
                                shader_location: 1,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 32,
                                shader_location: 2,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 48,
                                shader_location: 3,
                            },
                        ],
                    })],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &composite_shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: fmt,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        let mut r = Self {
            device,
            queue,
            target,
            uniform_buf,
            instance_buf,
            instance_capacity,
            backdrop_pipeline,
            backdrop_bg,
            blur_down_pipeline,
            blur_up_pipeline,
            blur_bgl,
            composite_pipeline,
            composite_bgl,
            composite_bg: None,
            sampler,
            font_view,
            targets: Vec::new(),
            down_bgs: Vec::new(),
            up_bgs: Vec::new(),
            surface_lost: false,
        };
        r.rebuild_targets();
        r
    }

    fn fullscreen_pipeline(
        device: &wgpu::Device,
        shader: &wgpu::ShaderModule,
        vs: &str,
        fs: &str,
        bgl: &wgpu::BindGroupLayout,
        format: wgpu::TextureFormat,
        label: &str,
    ) -> wgpu::RenderPipeline {
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(label),
            bind_group_layouts: &[Some(bgl)],
            immediate_size: 0,
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some(vs),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some(fs),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
    }

    fn upload_font(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::TextureView {
        let pixels = font::build_atlas();
        let tex = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("font-atlas"),
                size: wgpu::Extent3d {
                    width: font::ATLAS_W,
                    height: font::ATLAS_H,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &pixels,
        );
        tex.create_view(&Default::default())
    }

    /// (Re)create the blur chain and the bind groups that reference it.
    fn rebuild_targets(&mut self) {
        self.targets.clear();
        self.down_bgs.clear();
        self.up_bgs.clear();

        let make = |device: &wgpu::Device, w: u32, h: u32, label: &str| -> BlurTarget {
            let t = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // Rgba16Float, not Rgba8: the blur averages many samples of a
                // dark gradient, and 8-bit precision banding survives the
                // dither once it has been through five passes.
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            BlurTarget {
                view: t.create_view(&Default::default()),
                width: w.max(1),
                height: h.max(1),
            }
        };

        // Level 0 is full resolution; each subsequent level halves.
        for i in 0..=BLUR_LEVELS {
            let (tw, th) = self.target.size();
            let w = (tw >> i).max(1);
            let h = (th >> i).max(1);
            self.targets
                .push(make(&self.device, w, h, &format!("blur-{i}")));
        }

        // Down: level i -> level i+1.
        for i in 0..BLUR_LEVELS {
            let src = &self.targets[i];
            let params = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("blur-down-params"),
                contents: bytemuck::bytes_of(&BlurParams {
                    texel: [1.0 / src.width as f32, 1.0 / src.height as f32],
                    offset: 1.0,
                    _pad: 0.0,
                }),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            // `params` is dropped at the end of this iteration and that is
            // correct: wgpu bind groups hold internal reference counts on the
            // resources they bind, so the buffer outlives this handle. Leaking
            // it deliberately (mem::forget) would leak one buffer per level on
            // every window resize.
            self.down_bgs.push(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("blur-down-bg"),
                layout: &self.blur_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src.view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 2, resource: params.as_entire_binding() },
                ],
            }));
        }

        // Up: level i+1 -> level i, walking back down to level 1.
        for i in (1..BLUR_LEVELS).rev() {
            let src = &self.targets[i + 1];
            let params = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("blur-up-params"),
                contents: bytemuck::bytes_of(&BlurParams {
                    texel: [1.0 / src.width as f32, 1.0 / src.height as f32],
                    offset: 1.0,
                    _pad: 0.0,
                }),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            self.up_bgs.push(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("blur-up-bg"),
                layout: &self.blur_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src.view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 2, resource: params.as_entire_binding() },
                ],
            }));
        }

        // The composite pass samples level 1 — half resolution. There is no
        // point compositing against full res: the content is already blurred
        // far beyond one pixel of detail, and half res quarters the bandwidth.
        self.composite_bg = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite-bg"),
            layout: &self.composite_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.uniform_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&self.targets[1].view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&self.font_view) },
            ],
        }));
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        match &mut self.target {
            Target::Window { surface, config } => {
                config.width = width;
                config.height = height;
                surface.configure(&self.device, config);
            }
            // An offscreen target is fixed at construction; resizing it would
            // invalidate the buffer the caller is about to read.
            Target::Offscreen { .. } => return,
        }
        self.rebuild_targets();
    }

    pub fn size(&self) -> (f32, f32) {
        let (w, h) = self.target.size();
        (w as f32, h as f32)
    }

    fn ensure_instance_capacity(&mut self, needed: usize) {
        if needed <= self.instance_capacity {
            return;
        }
        let new_cap = needed.next_power_of_two();
        self.instance_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ui-instances"),
            size: (new_cap * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.instance_capacity = new_cap;
    }

    pub fn render(&mut self, uniforms: &Uniforms, instances: &[Instance]) {
        self.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(uniforms));

        self.ensure_instance_capacity(instances.len());
        if !instances.is_empty() {
            self.queue
                .write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(instances));
        }

        // Acquire the frame's colour target. For a window this is a
        // swapchain image that must be presented; offscreen it is just a view.
        use wgpu::CurrentSurfaceTexture as Cst;
        let mut frame: Option<wgpu::SurfaceTexture> = None;
        let view = match &self.target {
            Target::Window { surface, config } => {
                let acquired = match surface.get_current_texture() {
                    Cst::Success(f) => f,
                    // Suboptimal still hands us a usable texture — present it
                    // rather than dropping a frame; the next resize fixes it.
                    Cst::Suboptimal(f) => f,
                    Cst::Outdated | Cst::Lost => {
                        // Resized, or moved between monitors. Reconfigure and
                        // skip this frame rather than presenting a stale one.
                        surface.configure(&self.device, config);
                        return;
                    }
                    // Minimised or fully covered: nothing to present, and
                    // spinning would burn a core for no pixels.
                    Cst::Occluded | Cst::Timeout => return,
                    Cst::Validation => {
                        eprintln!("[gfx] surface validation error; stopping render");
                        self.surface_lost = true;
                        return;
                    }
                };
                let v = acquired.texture.create_view(&Default::default());
                frame = Some(acquired);
                v
            }
            // The offscreen view is created once at construction; there is no
            // per-frame acquisition to do.
            Target::Offscreen { view, .. } => view.clone(),
        };
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });

        // ---- pass 1: backdrop ------------------------------------------------
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("backdrop"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.targets[0].view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.backdrop_pipeline);
            rp.set_bind_group(0, &self.backdrop_bg, &[]);
            rp.draw(0..3, 0..1);
        }

        // ---- pass 2: blur chain ----------------------------------------------
        for i in 0..BLUR_LEVELS {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blur-down"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.targets[i + 1].view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.blur_down_pipeline);
            rp.set_bind_group(0, &self.down_bgs[i], &[]);
            rp.draw(0..3, 0..1);
        }
        for (n, i) in (1..BLUR_LEVELS).rev().enumerate() {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blur-up"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.targets[i].view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rp.set_pipeline(&self.blur_up_pipeline);
            rp.set_bind_group(0, &self.up_bgs[n], &[]);
            rp.draw(0..3, 0..1);
        }

        // ---- pass 3: composite ------------------------------------------------
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.024,
                            b: 0.033,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let Some(bg) = &self.composite_bg {
                rp.set_pipeline(&self.composite_pipeline);
                rp.set_bind_group(0, bg, &[]);
                rp.set_vertex_buffer(0, self.instance_buf.slice(..));
                rp.draw(0..6, 0..instances.len() as u32);
            }
        }

        self.queue.submit(Some(enc.finish()));
        // wgpu 30 presents through the queue rather than the texture.
        if let Some(f) = frame {
            self.queue.present(f);
        }
    }

    /// Read the offscreen target back as tightly packed RGBA8.
    ///
    /// Only valid on a headless renderer. Handles the 256-byte row alignment
    /// `copy_texture_to_buffer` requires, which is the usual source of
    /// diagonally-skewed screenshots.
    pub fn read_pixels(&self) -> anyhow_lite::Result<(u32, u32, Vec<u8>)> {
        let Target::Offscreen { texture, width, height, .. } = &self.target else {
            return Err("read_pixels requires a headless renderer".into());
        };
        let (width, height) = (*width, *height);

        const ALIGN: u32 = 256;
        let unpadded = width * 4;
        let padded = unpadded.div_ceil(ALIGN) * ALIGN;

        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(enc.finish()));

        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| format!("poll: {e:?}"))?;
        rx.recv()
            .map_err(|e| format!("map channel: {e}"))?
            .map_err(|e| format!("map_async: {e:?}"))?;

        let data = buffer
            .get_mapped_range(..)
            .map_err(|e| format!("get_mapped_range: {e:?}"))?;
        let mut out = Vec::with_capacity((unpadded * height) as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            out.extend_from_slice(&data[start..start + unpadded as usize]);
        }
        drop(data);
        buffer.unmap();
        Ok((width, height, out))
    }
}

/// A three-line stand-in for `anyhow`. The only thing this binary needs from
/// an error type is "a message that reaches `main`", and that does not justify
/// a dependency.
pub mod anyhow_lite {
    pub type Result<T> = std::result::Result<T, Error>;

    #[derive(Debug)]
    pub struct Error(pub String);

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Error {}
    impl From<String> for Error {
        fn from(s: String) -> Self {
            Error(s)
        }
    }
    impl From<&str> for Error {
        fn from(s: &str) -> Self {
            Error(s.to_string())
        }
    }
}
