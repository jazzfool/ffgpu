use std::{
    borrow::Cow,
    sync::Arc,
    time::{Duration, Instant},
};
use winit::{
    dpi::PhysicalSize,
    event::{ElementState, Event, WindowEvent},
    event_loop::EventLoop,
    keyboard::{KeyCode, PhysicalKey},
    window::WindowBuilder,
};

fn main() {
    env_logger::init();

    let event_loop = EventLoop::new().unwrap();

    let window = WindowBuilder::new()
        .with_title("winit player")
        .build(&event_loop)
        .unwrap();
    let window = Arc::new(window);

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..wgpu::InstanceDescriptor::new_without_display_handle_from_env()
    });

    let surface = instance.create_surface(&window).unwrap();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: Some(&surface),
    }))
    .unwrap();

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_features: ffgpu::required_wgpu_device_features(&adapter),
        ..Default::default()
    }))
    .unwrap();

    let path = std::env::args().nth(1).expect("no path given");
    let mut context = ffgpu::Context::new(&instance, &adapter, &device, &queue).unwrap();
    let (mut video, audio_sink) = context.create_video(&path).unwrap();
    let audio_sink = audio_sink.into_device_sink();

    let _ = window.request_inner_size(PhysicalSize::new(video.width(), video.height()));

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("shader.wgsl"))),
    });

    let bg0_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
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
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&bg0_layout)],
        immediate_size: 0,
    });

    let swapchain_capabilities = surface.get_capabilities(&adapter);
    let swapchain_format = swapchain_capabilities.formats[0];

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(swapchain_format.into())],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let mut config = surface
        .get_default_config(
            &adapter,
            window.inner_size().width,
            window.inner_size().height,
        )
        .unwrap();
    config.present_mode = wgpu::PresentMode::AutoVsync;
    surface.configure(&device, &config);

    let video_texture = video.texture().clone();
    let video_view = video_texture.create_view(&wgpu::TextureViewDescriptor {
        format: Some(wgpu::TextureFormat::Rgba8UnormSrgb),
        ..Default::default()
    });

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    let bg0 = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &bg0_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&video_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });

    let mut font_system = glyphon::FontSystem::new();
    let mut swash_cache = glyphon::SwashCache::new();
    let text_cache = glyphon::Cache::new(&device);
    let mut text_viewport = glyphon::Viewport::new(&device, &text_cache);
    let mut text_atlas = glyphon::TextAtlas::new(&device, &queue, &text_cache, swapchain_format);
    let mut text_renderer =
        glyphon::TextRenderer::new(&mut text_atlas, &device, Default::default(), None);
    let mut text_buffer_1 =
        glyphon::Buffer::new(&mut font_system, glyphon::Metrics::new(12.0, 14.0));
    let mut text_buffer_2 =
        glyphon::Buffer::new(&mut font_system, glyphon::Metrics::new(12.0, 14.0));

    text_buffer_2.set_text(
        &mut font_system,
        "[spacebar]: toggle pause\n[←→]: seek (5s)\n[↑↓] adjust volume\n[L]: toggle looping\n[>]: step one frame",
        &glyphon::Attrs::new().family(glyphon::Family::Monospace),
        glyphon::Shaping::Basic,
        None,
    );
    text_buffer_2.shape_until_scroll(&mut font_system, false);

    let mut wait_until = Instant::now();
    let mut next_text_update = Instant::now();

    let window = &window;
    event_loop
        .run(move |event, target| match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => target.exit(),
                WindowEvent::RedrawRequested => {
                    window.request_redraw();

                    let now = Instant::now();
                    if now < wait_until {
                        std::thread::sleep(wait_until - now);
                        return;
                    }

                    let frame = match surface.get_current_texture() {
                        wgpu::CurrentSurfaceTexture::Success(texture) => texture,
                        x => {
                            drop(x);
                            config.width = window.inner_size().width.max(1);
                            config.height = window.inner_size().height.max(1);
                            surface.configure(&device, &config);
                            window.request_redraw();
                            return;
                        }
                    };
                    let frame_view = frame.texture.create_view(&Default::default());

                    let mut encoder = device.create_command_encoder(&Default::default());

                    let wait = video.update(&mut encoder).expect("Video::update");
                    wait_until = now + wait;
                    let stats = video.statistics();

                    text_buffer_1.set_size(
                        &mut font_system,
                        Some(window.inner_size().width as f32 * window.scale_factor() as f32),
                        Some(window.inner_size().height as f32 * window.scale_factor() as f32),
                    );
                    text_buffer_2.set_size(
                        &mut font_system,
                        Some(window.inner_size().width as f32 * window.scale_factor() as f32),
                        Some(window.inner_size().height as f32 * window.scale_factor() as f32),
                    );

                    if now >= next_text_update {
                        next_text_update = now + Duration::from_millis(50);

                        text_buffer_1.set_text(
                            &mut font_system,
                            &format!(
                                "{}\n{}x{}@{:.02}fps\n{}\n{}\nlooping {}\n\n{:0>2}:{:0>2}.{:0>3}\n{:.0}% volume\ndelay {:#?}\nvideo clk: {:.03}\naudio clk: {:.03}\na/v sync: {:+04.0}ms",
                                &path,
                                video.width(),
                                video.height(),
                                video.framerate(),
                                video.decoder_name(),
                                if video.paused() { "paused" } else { "playing" },
                                if video.looping() { "on" } else { "off" },
                                video.position().as_secs() / 60,
                                video.position().as_secs() % 60,
                                video.position().as_millis() % 1000,
                                (audio_sink.gain() * 100.).round(),
                                wait,
                                stats.video_clock,
                                stats.audio_clock,
                                (stats.sync_latency * 1000.).round(),
                            ),
                            &glyphon::Attrs::new().family(glyphon::Family::Monospace),
                            glyphon::Shaping::Basic,
                            None,
                        );
                        text_buffer_1.shape_until_scroll(&mut font_system, false);
                    }

                    text_viewport.update(
                        &queue,
                        glyphon::Resolution {
                            width: config.width,
                            height: config.height,
                        },
                    );
                    text_renderer
                        .prepare(
                            &device,
                            &queue,
                            &mut font_system,
                            &mut text_atlas,
                            &text_viewport,
                            [
                                glyphon::TextArea {
                                    buffer: &text_buffer_1,
                                    left: 10.0,
                                    top: 10.0,
                                    scale: window.scale_factor() as _,
                                    bounds: glyphon::TextBounds {
                                        left: 10,
                                        top: 10,
                                        right: (190. * window.scale_factor()) as _,
                                        bottom: i32::MAX,
                                    },
                                    default_color: glyphon::Color::rgb(255, 255, 255),
                                    custom_glyphs: &[],
                                },
                                glyphon::TextArea {
                                    buffer: &text_buffer_2,
                                    left: (200.0 * window.scale_factor()) as _,
                                    top: 10.0,
                                    scale: window.scale_factor() as _,
                                    bounds: Default::default(),
                                    default_color: glyphon::Color::rgb(255, 255, 255),
                                    custom_glyphs: &[],
                                },
                            ],
                            &mut swash_cache,
                        )
                        .unwrap();

                    {
                        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: None,
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &frame_view,
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
                        });
                        rpass.set_pipeline(&pipeline);
                        rpass.set_bind_group(0, &bg0, &[]);

                        let ar = video.width() as f32 / video.height() as f32;
                        let ww = config.width as f32;
                        let wh = config.height as f32;
                        let (vw, vh) = if (ww / wh) > ar { (ar * wh, wh) } else { (ww, ww / ar) };
                        rpass.set_viewport((ww - vw) / 2., (wh - vh) / 2., vw, vh, 0.0, 1.0);

                        rpass.draw(0..3, 0..1);

                        rpass.set_viewport(0.0, 0.0, config.width as _, config.height as _, 0.0, 1.0);
                        text_renderer
                            .render(&text_atlas, &text_viewport, &mut rpass)
                            .unwrap();
                    }
                    queue.submit(Some(encoder.finish()));
                    window.pre_present_notify();
                    frame.present();
                }
                WindowEvent::KeyboardInput { event, .. }
                    if event.state == ElementState::Pressed =>
                {
                    match event.physical_key {
                        PhysicalKey::Code(KeyCode::Space) => {
                            video.set_paused(!video.paused());
                        }
                        PhysicalKey::Code(KeyCode::ArrowLeft) => video.seek(
                            video.position() - Duration::from_secs(5).min(video.position()), ffgpu::SeekMode::Accurate),
                        PhysicalKey::Code(KeyCode::ArrowRight) => {
                            video.seek(video.position() + Duration::from_secs(5), ffgpu::SeekMode::Accurate)
                        }
                        PhysicalKey::Code(KeyCode::ArrowUp) => {
                            audio_sink.set_gain((audio_sink.gain() + 0.1).clamp(0., 1.));
                        }
                        PhysicalKey::Code(KeyCode::ArrowDown) => {
                            audio_sink.set_gain((audio_sink.gain() - 0.1).clamp(0., 1.));
                        }
                        PhysicalKey::Code(KeyCode::Period) => {
                            video.step_one_frame();
                        }
                        PhysicalKey::Code(KeyCode::KeyL) => {
                            video.set_looping(!video.looping());
                        }
                        _ => {}
                    }
                }
                _ => {}
            },
            _ => {}
        })
        .unwrap();
}
