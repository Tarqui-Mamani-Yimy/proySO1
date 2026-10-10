use cosmic_text::{Attrs, Buffer, Family, Metrics, Weight};
use glyphon::{
    Cache, Color, FontSystem, Resolution, SwashCache, TextArea, TextAtlas, TextBounds,
    TextRenderer, Viewport,
};
use so1_terminal::{
    clamp_surface_size,
    keyboard::{Key as TerminalKey, Modifiers as KeyModifiers, encode_ime_commit, encode_key},
    model::{Model, ModelEvent},
    renderer::{
        ExitPollBackoff, ExitPollDecision, FrameRecovery, SurfaceFailure, WakeupGate, grid_size,
        scaled_cell_metrics,
    },
};
use std::{path::PathBuf, sync::Arc, time::Instant};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
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
    modifiers: winit::keyboard::ModifiersState,
    ime_preedit_active: bool,
    proxy: EventLoopProxy<UserEvent>,
    pty_wakeup: Arc<WakeupGate>,
    exit_poll: ExitPollBackoff,
    next_exit_poll: Instant,
    retry_redraw_at: Option<Instant>,
    occluded: bool,
    redraw_pending: bool,
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
    logical_cell_width: f32,
    logical_cell_height: f32,
    rect_pipeline: wgpu::RenderPipeline,
    line_buffers: Vec<Buffer>,
    vertices: Vec<RectVertex>,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RectVertex {
    position: [f32; 2],
    color: [f32; 3],
}
impl TerminalWindow {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let now = Instant::now();
        Self {
            window: None,
            gpu: None,
            model: None,
            pty: None,
            failed: false,
            modifiers: winit::keyboard::ModifiersState::empty(),
            ime_preedit_active: false,
            proxy,
            pty_wakeup: Arc::new(WakeupGate::default()),
            exit_poll: ExitPollBackoff::new(now),
            next_exit_poll: now,
            retry_redraw_at: None,
            occluded: false,
            redraw_pending: false,
        }
    }
    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let window = Arc::new(
            event_loop
                .create_window(WindowAttributes::default().with_title("so1term"))
                .map_err(|e| format!("could not create so1term window: {e}"))?,
        );
        window.set_ime_allowed(true);
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
        let vertex_capacity = 1024;
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terminal reusable cell vertex buffer"),
            size: vertex_capacity as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let (physical_cell_width, physical_cell_height) =
            scaled_cell_metrics(cell_width, cell_height, window.scale_factor());
        let (cols, rows) = grid_size(width, height, physical_cell_width, physical_cell_height);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let proxy = self.proxy.clone();
        let pty_wakeup = Arc::clone(&self.pty_wakeup);
        let pty = so1_terminal::pty::Pty::spawn_shell_notified(
            cwd,
            cols as u16,
            rows as u16,
            move || {
                if pty_wakeup.request() {
                    let _ = proxy.send_event(UserEvent::PtyOutput);
                }
            },
        )
        .map_err(|e| format!("could not start shell PTY: {e}"))?;
        self.model = Some(Model::new(cols, rows));
        self.pty = Some(pty);
        self.window = Some(Arc::clone(&window));
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
            cell_width: physical_cell_width,
            cell_height: physical_cell_height,
            logical_cell_width: cell_width,
            logical_cell_height: cell_height,
            rect_pipeline,
            line_buffers: Vec::new(),
            vertices: Vec::new(),
            vertex_buffer,
            vertex_capacity,
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
    fn request_redraw(&mut self) {
        if self.redraw_pending || self.occluded {
            return;
        }
        if let Some(window) = self.window.as_ref() {
            self.redraw_pending = true;
            window.request_redraw();
        }
    }

    fn poll_child_exit(&mut self, event_loop: &ActiveEventLoop) {
        let status = self
            .pty
            .as_mut()
            .and_then(|pty| pty.try_wait().ok().flatten());
        match self
            .exit_poll
            .observe_wait_status(status.is_some(), Instant::now())
        {
            ExitPollDecision::Exit => {
                self.pty.take();
                event_loop.exit();
            }
            ExitPollDecision::RetryAt(deadline) if self.pty.is_some() => {
                self.next_exit_poll = deadline;
            }
            ExitPollDecision::RetryAt(_) => {}
        }
    }

    fn draw(&mut self) -> Result<FrameRecovery, String> {
        let (Some(gpu), Some(model)) = (self.gpu.as_mut(), self.model.as_ref()) else {
            return Ok(FrameRecovery::WaitForEvent);
        };
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Lost => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return Ok(so1_terminal::renderer::frame_recovery(SurfaceFailure::Lost));
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return Ok(so1_terminal::renderer::frame_recovery(
                    SurfaceFailure::Outdated,
                ));
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                return Ok(so1_terminal::renderer::frame_recovery(
                    SurfaceFailure::Timeout,
                ));
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(so1_terminal::renderer::frame_recovery(
                    SurfaceFailure::Occluded,
                ));
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(
                    "GPU surface reported a validation error while acquiring a frame".into(),
                );
            }
        };

        let rows = model.visible_cells();
        let cursor = model.cursor();
        let cells = so1_terminal::renderer::render_cells(&rows, (cursor.column, cursor.row));
        let lines = so1_terminal::renderer::styled_lines(&cells);
        let width = gpu.config.width as f32;
        let height = gpu.config.height as f32;
        gpu.vertices.clear();
        for (row, cells) in cells.iter().enumerate() {
            for (col, cell) in cells.iter().enumerate() {
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
                    gpu.vertices.push(RectVertex {
                        position: points[point],
                        color,
                    });
                }
            }
        }

        while gpu.line_buffers.len() < lines.len() {
            gpu.line_buffers.push(Buffer::new(
                &mut gpu.font_system,
                Metrics::new(gpu.cell_height / 1.35, gpu.cell_height),
            ));
        }
        for (row, runs) in lines.iter().enumerate() {
            if runs
                .iter()
                .all(|run| run.text.chars().all(char::is_whitespace))
            {
                continue;
            }
            let buffer = &mut gpu.line_buffers[row];
            buffer.set_size(Some(width), Some(gpu.cell_height));
            let default_attrs = Attrs::new().family(Family::Monospace);
            let spans = runs.iter().map(|run| {
                let attrs = Attrs::new()
                    .family(Family::Monospace)
                    .color(Color::rgb(
                        run.foreground[0],
                        run.foreground[1],
                        run.foreground[2],
                    ))
                    .weight(if run.bold {
                        Weight::BOLD
                    } else {
                        Weight::NORMAL
                    });
                (run.text.as_str(), attrs)
            });
            buffer.set_rich_text(spans, &default_attrs, cosmic_text::Shaping::Advanced, None);
            buffer.shape_until_scroll(&mut gpu.font_system, true);
        }
        gpu.viewport.update(
            &gpu.queue,
            Resolution {
                width: gpu.config.width,
                height: gpu.config.height,
            },
        );
        let areas: Vec<_> = lines
            .iter()
            .enumerate()
            .filter_map(|(row, runs)| {
                if runs
                    .iter()
                    .all(|run| run.text.chars().all(char::is_whitespace))
                {
                    return None;
                }
                Some(TextArea {
                    buffer: &gpu.line_buffers[row],
                    left: 0.0,
                    top: row as f32 * gpu.cell_height,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: gpu.config.width as i32,
                        bottom: gpu.config.height as i32,
                    },
                    default_color: Color::rgb(220, 220, 220),
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

        let required = gpu.vertices.len() * std::mem::size_of::<RectVertex>();
        let capacity = so1_terminal::renderer::next_vertex_capacity(gpu.vertex_capacity, required);
        if capacity > gpu.vertex_capacity {
            gpu.vertex_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("terminal reusable cell vertex buffer"),
                size: capacity as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.vertex_capacity = capacity;
        }
        if required > 0 {
            gpu.queue
                .write_buffer(&gpu.vertex_buffer, 0, bytemuck::cast_slice(&gpu.vertices));
        }

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
            if required > 0 {
                pass.set_pipeline(&gpu.rect_pipeline);
                pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
                pass.draw(0..gpu.vertices.len() as u32, 0..1);
            }
            gpu.text_renderer
                .render(&gpu.atlas, &gpu.viewport, &mut pass)
                .map_err(|e| format!("could not render terminal text: {e}"))?;
        }
        gpu.queue.submit([encoder.finish()]);
        gpu.queue.present(frame);
        Ok(FrameRecovery::WaitForEvent)
    }
    fn receive_output(&mut self, event_loop: &ActiveEventLoop) {
        let mut changed = false;
        if let Some(pty) = self.pty.as_mut() {
            for bytes in pty.drain_output() {
                changed = true;
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
        }
        self.poll_child_exit(event_loop);
        if changed {
            self.request_redraw();
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
        self.request_redraw();
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: UserEvent) {
        self.pty_wakeup.acknowledge();
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
                self.request_redraw();
            }
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                if !occluded {
                    self.retry_redraw_at = None;
                    self.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.redraw_pending = false;
                if self.occluded {
                    return;
                }
                match self.draw() {
                    Ok(FrameRecovery::ReconfigureAndRedraw) => self.request_redraw(),
                    Ok(FrameRecovery::RetryAfter(delay)) => {
                        self.retry_redraw_at = Some(Instant::now() + delay)
                    }
                    Ok(FrameRecovery::WaitForEvent) => {}
                    Err(e) => {
                        eprintln!("so1term: {e}");
                        self.failed = true;
                        event_loop.exit();
                    }
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::Ime(winit::event::Ime::Preedit(preedit, _)) => {
                self.ime_preedit_active = !preedit.is_empty();
            }
            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                if let Some(model) = self.model.as_mut() {
                    let bytes = encode_ime_commit(&text, model.bracketed_paste_enabled());
                    if let Some(pty) = self.pty.as_mut() {
                        let _ = pty.write(&bytes);
                    }
                    model.scroll_to_bottom();
                }
                self.request_redraw();
                self.ime_preedit_active = false;
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let mods = KeyModifiers {
                    shift: self.modifiers.contains(ModifiersState::SHIFT),
                    alt: self.modifiers.contains(ModifiersState::ALT),
                    control: self.modifiers.contains(ModifiersState::CONTROL),
                    super_key: self.modifiers.contains(ModifiersState::SUPER),
                };
                let terminal_key = match &event.logical_key {
                    Key::Named(NamedKey::Enter) => Some(TerminalKey::Enter),
                    Key::Named(NamedKey::Tab) => Some(TerminalKey::Tab),
                    Key::Named(NamedKey::Backspace) => Some(TerminalKey::Backspace),
                    Key::Named(NamedKey::Escape) => Some(TerminalKey::Escape),
                    Key::Named(NamedKey::ArrowUp) => Some(TerminalKey::Up),
                    Key::Named(NamedKey::ArrowDown) => Some(TerminalKey::Down),
                    Key::Named(NamedKey::ArrowLeft) => Some(TerminalKey::Left),
                    Key::Named(NamedKey::ArrowRight) => Some(TerminalKey::Right),
                    Key::Named(NamedKey::Home) => Some(TerminalKey::Home),
                    Key::Named(NamedKey::End) => Some(TerminalKey::End),
                    Key::Named(NamedKey::PageUp) => Some(TerminalKey::PageUp),
                    Key::Named(NamedKey::PageDown) => Some(TerminalKey::PageDown),
                    Key::Named(NamedKey::Insert) => Some(TerminalKey::Insert),
                    Key::Named(NamedKey::Delete) => Some(TerminalKey::Delete),
                    Key::Named(NamedKey::F1) => Some(TerminalKey::Function(1)),
                    Key::Named(NamedKey::F2) => Some(TerminalKey::Function(2)),
                    Key::Named(NamedKey::F3) => Some(TerminalKey::Function(3)),
                    Key::Named(NamedKey::F4) => Some(TerminalKey::Function(4)),
                    Key::Named(NamedKey::F5) => Some(TerminalKey::Function(5)),
                    Key::Named(NamedKey::F6) => Some(TerminalKey::Function(6)),
                    Key::Named(NamedKey::F7) => Some(TerminalKey::Function(7)),
                    Key::Named(NamedKey::F8) => Some(TerminalKey::Function(8)),
                    Key::Named(NamedKey::F9) => Some(TerminalKey::Function(9)),
                    Key::Named(NamedKey::F10) => Some(TerminalKey::Function(10)),
                    Key::Named(NamedKey::F11) => Some(TerminalKey::Function(11)),
                    Key::Named(NamedKey::F12) => Some(TerminalKey::Function(12)),
                    _ => event
                        .text
                        .as_ref()
                        .filter(|text| !text.is_empty())
                        .map(|text| TerminalKey::Text(text.as_str())),
                };
                if !self.ime_preedit_active
                    && let (Some(key), Some(model)) = (terminal_key, self.model.as_mut())
                {
                    let app_cursor = model.application_cursor_keys();
                    if let Some(bytes) = encode_key(key, mods, app_cursor)
                        && let Some(pty) = self.pty.as_mut()
                    {
                        let _ = pty.write(&bytes);
                    }
                    model.scroll_to_bottom();
                }
                self.request_redraw();
            }
            WindowEvent::Ime(winit::event::Ime::Enabled) => {}
            WindowEvent::Ime(winit::event::Ime::Disabled) => self.ime_preedit_active = false,
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(model) = self.model.as_mut()
                    && !model.mouse_or_alternate_screen_active()
                {
                    let amount = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y.abs().ceil() as usize,
                        MouseScrollDelta::PixelDelta(pos) => (pos.y.abs() / 16.0).ceil() as usize,
                    }
                    .max(1);
                    if match delta {
                        MouseScrollDelta::LineDelta(_, y) => y > 0.0,
                        MouseScrollDelta::PixelDelta(pos) => pos.y > 0.0,
                    } {
                        model.scroll_up(amount);
                    } else {
                        model.scroll_down(amount);
                    }
                    self.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(gpu) = self.gpu.as_mut() {
                    (gpu.cell_width, gpu.cell_height) = scaled_cell_metrics(
                        gpu.logical_cell_width,
                        gpu.logical_cell_height,
                        scale_factor,
                    );
                }
                if let Some(window) = self.window.as_ref() {
                    let size = window.inner_size();
                    self.resize(size.width, size.height);
                    self.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self.pty.is_some() && now >= self.next_exit_poll {
            self.poll_child_exit(event_loop);
        }
        if self.retry_redraw_at.is_some_and(|deadline| deadline <= now) {
            self.retry_redraw_at = None;
            self.request_redraw();
        }
        let next_deadline = [
            self.pty.is_some().then_some(self.next_exit_poll),
            self.retry_redraw_at,
        ]
        .into_iter()
        .flatten()
        .min();
        event_loop.set_control_flow(next_deadline.map_or(
            winit::event_loop::ControlFlow::Wait,
            winit::event_loop::ControlFlow::WaitUntil,
        ));
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
