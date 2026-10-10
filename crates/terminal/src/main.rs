use cosmic_text::{Attrs, Buffer, Family, Metrics, Weight};
use glyphon::{
    Cache, Color, FontSystem, Resolution, SwashCache, TextArea, TextAtlas, TextBounds,
    TextRenderer, Viewport,
};
use so1_terminal::{
    clamp_surface_size,
    model::{Model, ModelEvent},
    renderer::grid_size,
};
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    keyboard::{Key, NamedKey},
    window::{Window, WindowAttributes, WindowId},
};

#[derive(Clone, Copy)]
enum UserEvent {
    PtyOutput,
}
struct TerminalWindow {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    model: Option<Model>,
    pty: Option<so1_terminal::pty::Pty>,
    failed: bool,
    input: String,
    proxy: EventLoopProxy<UserEvent>,
}
struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    font_system: FontSystem,
    swash_cache: SwashCache,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    viewport: Viewport,
    cell_width: f32,
    cell_height: f32,
    rect_pipeline: wgpu::RenderPipeline,
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RectVertex {
    position: [f32; 2],
    color: [f32; 3],
}
impl TerminalWindow {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            window: None,
            gpu: None,
            model: None,
            pty: None,
            failed: false,
            input: String::new(),
            proxy,
        }
    }
    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let window = Arc::new(
            event_loop
                .create_window(WindowAttributes::default().with_title("so1term"))
                .map_err(|e| format!("could not create so1term window: {e}"))?,
        );
        let size = window.inner_size();
        let (width, height) = clamp_surface_size(size.width, size.height);
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("could not create GPU surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| {
            format!("could not find a GPU adapter compatible with the window surface: {e}")
        })?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("so1term device"),
            ..Default::default()
        }))
        .map_err(|e| format!("could not initialize GPU device: {e}"))?;
        let config = surface
            .get_default_config(&adapter, width, height)
            .ok_or_else(|| "GPU adapter does not support the window surface".to_owned())?;
        surface.configure(&device, &config);
        let mut font_system = FontSystem::new();
        if let Ok(font) = std::env::var("SO1TERM_FONT") {
            font_system.db_mut().load_font_file(font).ok();
        }
        let font_size = std::env::var("SO1TERM_FONT_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|value: &f32| value.is_finite() && (6.0..=72.0).contains(value))
            .unwrap_or(14.0);
        let metrics = Metrics::new(font_size, font_size * 1.35);
        let mut measure = Buffer::new(&mut font_system, metrics);
        measure.set_text(
            "M",
            &Attrs::new().family(Family::Monospace),
            cosmic_text::Shaping::Advanced,
            None,
        );
        measure.shape_until_scroll(&mut font_system, true);
        let (cell_width, cell_height) = measure
            .layout_runs()
            .next()
            .and_then(|run| {
                run.glyphs
                    .first()
                    .map(|glyph| (glyph.w.max(1.0), run.line_height.max(1.0)))
            })
            .unwrap_or((font_size * 0.6, font_size * 1.35));
        let cache = Cache::new(&device);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, config.format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let viewport = Viewport::new(&device, &cache);
        let swash_cache = SwashCache::new();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("terminal cell color shader"),
            source: wgpu::ShaderSource::Wgsl(r#"
                struct Out { @builtin(position) position: vec4<f32>, @location(0) color: vec3<f32> };
                @vertex fn vs_main(@location(0) p: vec2<f32>, @location(1) c: vec3<f32>) -> Out { var o: Out; o.position=vec4<f32>(p,0.0,1.0); o.color=c; return o; }
                @fragment fn fs_main(i: Out) -> @location(0) vec4<f32> { return vec4<f32>(i.color,1.0); }
            "#.into()),
        });
        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("terminal cell color pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<RectVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x3],
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let (cols, rows) = grid_size(width, height, cell_width, cell_height);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let proxy = self.proxy.clone();
        let pty = so1_terminal::pty::Pty::spawn_shell_notified(
            cwd,
            cols as u16,
            rows as u16,
            move || {
                let _ = proxy.send_event(UserEvent::PtyOutput);
            },
        )
        .map_err(|e| format!("could not start shell PTY: {e}"))?;
        self.model = Some(Model::new(cols, rows));
        self.pty = Some(pty);
        self.window = Some(window);
        self.gpu = Some(GpuState {
            surface,
            device,
            queue,
            config,
            font_system,
            swash_cache,
            atlas,
            text_renderer,
            viewport,
            cell_width,
            cell_height,
            rect_pipeline,
        });
        Ok(())
    }
    fn resize(&mut self, width: u32, height: u32) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        let (w, h) = clamp_surface_size(width, height);
        gpu.config.width = w;
        gpu.config.height = h;
        gpu.surface.configure(&gpu.device, &gpu.config);
        let (cols, rows) = grid_size(w, h, gpu.cell_width, gpu.cell_height);
        if let Some(m) = self.model.as_mut() {
            m.resize(cols, rows);
        }
        if let Some(p) = self.pty.as_ref() {
            let _ = p.resize(
                cols as u16,
                rows as u16,
                w.min(u16::MAX as u32) as u16,
                h.min(u16::MAX as u32) as u16,
            );
        }
    }
    fn draw(&mut self) -> Result<(), String> {
        let (Some(gpu), Some(model)) = (self.gpu.as_mut(), self.model.as_ref()) else {
            return Ok(());
        };
        let rows = model.visible_cells();
        let cursor = model.cursor();
        let cells = so1_terminal::renderer::render_cells(&rows, (cursor.column, cursor.row));
        let cols = rows.first().map_or(1, Vec::len).max(1);
        let width = gpu.config.width as f32;
        let height = gpu.config.height as f32;
        let mut vertices = Vec::with_capacity(cells.len() * 6);
        let mut text_buffers = Vec::with_capacity(cells.len());
        for (index, cell) in cells.iter().enumerate() {
            let col = index % cols;
            let row = index / cols;
            let x0 = col as f32 * gpu.cell_width;
            let y0 = row as f32 * gpu.cell_height;
            let x1 = x0 + gpu.cell_width;
            let y1 = y0 + gpu.cell_height;
            let points = [
                [x0 / width * 2.0 - 1.0, 1.0 - y0 / height * 2.0],
                [x1 / width * 2.0 - 1.0, 1.0 - y0 / height * 2.0],
                [x1 / width * 2.0 - 1.0, 1.0 - y1 / height * 2.0],
                [x0 / width * 2.0 - 1.0, 1.0 - y1 / height * 2.0],
            ];
            let color = cell.background.map(so1_terminal::renderer::srgb_to_linear);
            for point in [0, 1, 2, 0, 2, 3] {
                vertices.push(RectVertex {
                    position: points[point],
                    color,
                });
            }
            let mut buffer = Buffer::new(
                &mut gpu.font_system,
                Metrics::new(gpu.cell_height / 1.35, gpu.cell_height),
            );
            buffer.set_size(Some(gpu.cell_width), Some(gpu.cell_height));
            let attrs = Attrs::new()
                .family(Family::Monospace)
                .color(Color::rgb(
                    cell.foreground[0],
                    cell.foreground[1],
                    cell.foreground[2],
                ))
                .weight(if cell.bold {
                    Weight::BOLD
                } else {
                    Weight::NORMAL
                });
            buffer.set_text(
                &cell.character.to_string(),
                &attrs,
                cosmic_text::Shaping::Advanced,
                None,
            );
            buffer.shape_until_scroll(&mut gpu.font_system, true);
            text_buffers.push(buffer);
        }
        gpu.viewport.update(
            &gpu.queue,
            Resolution {
                width: gpu.config.width,
                height: gpu.config.height,
            },
        );
        let areas: Vec<_> = text_buffers
            .iter()
            .enumerate()
            .filter_map(|(index, buffer)| {
                let cell = cells[index];
                if cell.character == ' ' {
                    return None;
                }
                let col = index % cols;
                let row = index / cols;
                Some(TextArea {
                    buffer,
                    left: col as f32 * gpu.cell_width,
                    top: row as f32 * gpu.cell_height,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: gpu.config.width as i32,
                        bottom: gpu.config.height as i32,
                    },
                    default_color: Color::rgb(
                        cell.foreground[0],
                        cell.foreground[1],
                        cell.foreground[2],
                    ),
                    custom_glyphs: &[],
                })
            })
            .collect();
        gpu.text_renderer
            .prepare(
                &gpu.device,
                &gpu.queue,
                &mut gpu.font_system,
                &mut gpu.atlas,
                &gpu.viewport,
                areas,
                &mut gpu.swash_cache,
            )
            .map_err(|e| format!("could not prepare terminal text: {e}"))?;
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(
                    "GPU surface reported a validation error while acquiring a frame".into(),
                );
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("so1term renderer"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("terminal cell colors and text"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: so1_terminal::renderer::surface_clear_color()[0],
                            g: so1_terminal::renderer::surface_clear_color()[1],
                            b: so1_terminal::renderer::surface_clear_color()[2],
                            a: so1_terminal::renderer::surface_clear_color()[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if !vertices.is_empty() {
                let vertex_buffer = wgpu::util::DeviceExt::create_buffer_init(
                    &gpu.device,
                    &wgpu::util::BufferInitDescriptor {
                        label: Some("terminal cell backgrounds"),
                        contents: bytemuck::cast_slice(&vertices),
                        usage: wgpu::BufferUsages::VERTEX,
                    },
                );
                pass.set_pipeline(&gpu.rect_pipeline);
                pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                pass.draw(0..vertices.len() as u32, 0..1);
            }
            gpu.text_renderer
                .render(&gpu.atlas, &gpu.viewport, &mut pass)
                .map_err(|e| format!("could not render terminal text: {e}"))?;
        }
        gpu.queue.submit([encoder.finish()]);
        gpu.queue.present(frame);
        Ok(())
    }
    fn receive_output(&mut self, event_loop: &ActiveEventLoop) {
        let mut exited = false;
        if let Some(pty) = self.pty.as_mut() {
            for bytes in pty.drain_output() {
                if let Some(model) = self.model.as_mut() {
                    model.feed(&bytes);
                    for ev in model.take_events() {
                        match ev {
                            ModelEvent::PtyWrite(b) => {
                                let _ = pty.write(&b);
                            }
                            ModelEvent::Title(t) => {
                                if let Some(w) = &self.window {
                                    w.set_title(&t)
                                }
                            }
                            ModelEvent::ResetTitle => {
                                if let Some(w) = &self.window {
                                    w.set_title("so1term")
                                }
                            }
                            ModelEvent::Bell => {}
                            ModelEvent::Other(_) => {}
                        }
                    }
                }
            }
            if pty.try_wait().ok().flatten().is_some() {
                exited = true;
            }
        }
        if exited {
            self.pty.take();
            event_loop.exit();
        } else if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}
impl ApplicationHandler<UserEvent> for TerminalWindow {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.failed {
            return;
        }
        if let Err(e) = self.initialize(event_loop) {
            eprintln!("so1term: {e}");
            self.failed = true;
            event_loop.exit();
            return;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: UserEvent) {
        self.receive_output(event_loop)
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.pty.take();
                event_loop.exit()
            }
            WindowEvent::Resized(s) => {
                self.resize(s.width, s.height);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.draw() {
                    eprintln!("so1term: {e}");
                    self.failed = true;
                    event_loop.exit();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key {
                    Key::Named(NamedKey::Enter) => {
                        if let Some(p) = self.pty.as_mut() {
                            let _ = p.write(b"\r");
                        }
                        self.input.clear()
                    }
                    Key::Named(NamedKey::Backspace) => {
                        if !self.input.is_empty() {
                            self.input.pop();
                            if let Some(p) = self.pty.as_mut() {
                                let _ = p.write(b"\x7f");
                            }
                        }
                    }
                    Key::Character(s) => {
                        let bytes = s.as_bytes();
                        if let Some(p) = self.pty.as_mut() {
                            let _ = p.write(bytes);
                        }
                        self.input.push_str(&s)
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
fn main() {
    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("so1term: could not initialize the window event loop: {e}");
            std::process::exit(1)
        }
    };
    let mut app = TerminalWindow::new(event_loop.create_proxy());
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("so1term: window event loop failed: {e}");
        std::process::exit(1);
    }
    if app.failed {
        std::process::exit(1);
    }
}
