use alacritty_terminal::selection::SelectionType;
use arboard::Clipboard;
use cosmic_text::{Attrs, Buffer, Family, Metrics, Weight};
use glyphon::{
    Cache, Color, FontSystem, Resolution, SwashCache, TextArea, TextAtlas, TextBounds,
    TextRenderer, Viewport,
};
use so1_terminal::{
    clamp_surface_size,
    keyboard::{
        ImePreeditGate, Key as TerminalKey, Modifiers as KeyModifiers, encode_ime_commit,
        encode_key, encode_paste,
    },
    layout::{CellSize, Direction, FocusDirection, LayoutTree, PaneId, PaneLayout, Rect, Tab},
    model::{Model, ModelEvent},
    renderer::{
        BlockDecorationColor, BlockViewport, ExitPollBackoff, ExitPollDecision, FrameRecovery,
        SurfaceFailure, SurfaceRetryBackoff, WakeupGate, WheelAccumulator,
        apply_selection_highlight, collect_glyph_positions, grid_size, project_block_decorations,
        project_terminal_metrics, unicode_cell_width,
    },
    selection::{PointerAction, PointerButton, PointerState, SelectionGesture, SelectionRouter},
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Instant};
use winit::dpi::PhysicalPosition;
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowAttributes, WindowId},
};

#[derive(Clone, Copy)]
enum UserEvent {
    PtyOutput(PaneId),
}
struct TerminalWindow {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    tab: Option<Tab>,
    panes: BTreeMap<PaneId, Pane>,
    failed: bool,
    modifiers: winit::keyboard::ModifiersState,
    ime_gate: ImePreeditGate,
    surface_retry: SurfaceRetryBackoff,
    proxy: EventLoopProxy<UserEvent>,
    exit_poll: ExitPollBackoff,
    next_exit_poll: Instant,
    retry_redraw_at: Option<Instant>,
    occluded: bool,
    redraw_pending: bool,
    cursor_position: PhysicalPosition<f64>,
    clipboard: Option<Clipboard>,
}
struct Pane {
    pty: so1_terminal::pty::Pty,
    model: Model,
    wakeup: Arc<WakeupGate>,
    wheel: WheelAccumulator,
    selection_router: SelectionRouter,
    glyph_buffers: Vec<Buffer>,
    glyph_keys: Vec<Option<GlyphKey>>,
    glyph_positions: Vec<(usize, usize)>,
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
    font_size: f32,
    logical_font_size: f32,
    logical_cell_width: f32,
    logical_cell_height: f32,
    rect_pipeline: wgpu::RenderPipeline,
    vertices: Vec<RectVertex>,
    vertex_buffer: wgpu::Buffer,
    vertex_buffer_capacity_bytes: usize,
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RectVertex {
    position: [f32; 2],
    color: [f32; 3],
}

type GlyphKey = (char, [u8; 3], bool, usize);

fn overlay_vertex_range(
    background_vertices: usize,
    total_vertices: usize,
) -> Option<std::ops::Range<u32>> {
    (total_vertices > background_vertices)
        .then_some(background_vertices as u32..total_vertices as u32)
}

fn block_marker_bounds(cell_width: f32, cell_height: f32) -> [f32; 4] {
    let marker_width = cell_width.clamp(0.0, 2.5);
    let top = cell_height.clamp(0.0, 2.0);
    [
        cell_width - marker_width,
        top,
        cell_width,
        (cell_height - 2.0).max(top),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockShortcut {
    Previous,
    Next,
    Select,
    CopyOutput,
    CopyInput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneAction {
    SplitRight,
    SplitDown,
    Close,
    Focus(FocusDirection),
}

fn pane_action(key: &Key, mods: KeyModifiers, macos: bool) -> Option<PaneAction> {
    let command = if macos { mods.super_key } else { mods.control };
    let split = command && mods.shift;
    match key {
        Key::Character(text) if split && text.eq_ignore_ascii_case("d") => {
            Some(PaneAction::SplitRight)
        }
        Key::Character(text) if split && text.eq_ignore_ascii_case("e") => {
            Some(PaneAction::SplitDown)
        }
        Key::Character(text) if split && text.eq_ignore_ascii_case("w") => Some(PaneAction::Close),
        Key::Named(NamedKey::ArrowLeft) if command && mods.alt && !mods.shift => {
            Some(PaneAction::Focus(FocusDirection::Left))
        }
        Key::Named(NamedKey::ArrowRight) if command && mods.alt && !mods.shift => {
            Some(PaneAction::Focus(FocusDirection::Right))
        }
        Key::Named(NamedKey::ArrowUp) if command && mods.alt && !mods.shift => {
            Some(PaneAction::Focus(FocusDirection::Up))
        }
        Key::Named(NamedKey::ArrowDown) if command && mods.alt && !mods.shift => {
            Some(PaneAction::Focus(FocusDirection::Down))
        }
        _ => None,
    }
}

fn block_shortcut(key: &Key, mods: KeyModifiers, macos: bool) -> Option<BlockShortcut> {
    let control_shift = mods.control && mods.shift;
    match key {
        Key::Named(NamedKey::ArrowUp)
            if (mods.control && mods.shift) || (macos && mods.super_key && !mods.shift) =>
        {
            Some(BlockShortcut::Previous)
        }
        Key::Named(NamedKey::ArrowDown)
            if (mods.control && mods.shift) || (macos && mods.super_key && !mods.shift) =>
        {
            Some(BlockShortcut::Next)
        }
        Key::Character(text)
            if (control_shift || (macos && mods.super_key && mods.shift))
                && text.eq_ignore_ascii_case("a") =>
        {
            Some(BlockShortcut::Select)
        }
        Key::Character(text) if control_shift && text.eq_ignore_ascii_case("o") => {
            Some(BlockShortcut::CopyOutput)
        }
        Key::Character(text) if control_shift && text.eq_ignore_ascii_case("l") => {
            Some(BlockShortcut::CopyInput)
        }
        _ => None,
    }
}

fn route_block_shortcut(
    shortcut: Option<BlockShortcut>,
    alternate_screen_active: bool,
) -> Option<BlockShortcut> {
    (!alternate_screen_active).then_some(shortcut).flatten()
}

fn pane_cell_at(
    x: f64,
    y: f64,
    layouts: &[PaneLayout],
    cell_width: f32,
    cell_height: f32,
) -> Option<(PaneId, usize, usize)> {
    if !x.is_finite() || !y.is_finite() || cell_width <= 0.0 || cell_height <= 0.0 {
        return None;
    }
    let layout = layouts.iter().find(|layout| {
        !layout.hidden
            && x >= f64::from(layout.rect.x)
            && x < f64::from(layout.rect.right())
            && y >= f64::from(layout.rect.y)
            && y < f64::from(layout.rect.bottom())
    })?;
    let column = ((x - f64::from(layout.rect.x)) / f64::from(cell_width)).floor() as usize;
    let row = ((y - f64::from(layout.rect.y)) / f64::from(cell_height)).floor() as usize;
    Some((
        layout.pane,
        column.min(layout.cols.saturating_sub(1) as usize),
        row.min(layout.rows.saturating_sub(1) as usize),
    ))
}

#[allow(clippy::too_many_arguments)]
fn push_overlay_rect(
    vertices: &mut Vec<RectVertex>,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    viewport_width: f32,
    viewport_height: f32,
    color: [u8; 3],
) {
    if viewport_width <= 0.0 || viewport_height <= 0.0 || x1 <= x0 || y1 <= y0 {
        return;
    }
    let points = [
        [
            x0 / viewport_width * 2.0 - 1.0,
            1.0 - y0 / viewport_height * 2.0,
        ],
        [
            x1 / viewport_width * 2.0 - 1.0,
            1.0 - y0 / viewport_height * 2.0,
        ],
        [
            x1 / viewport_width * 2.0 - 1.0,
            1.0 - y1 / viewport_height * 2.0,
        ],
        [
            x0 / viewport_width * 2.0 - 1.0,
            1.0 - y1 / viewport_height * 2.0,
        ],
    ];
    let color = color.map(so1_terminal::renderer::srgb_to_linear);
    for point in [0, 1, 2, 0, 2, 3] {
        vertices.push(RectVertex {
            position: points[point],
            color,
        });
    }
}

fn glyph_key_requires_reshape(cached: Option<GlyphKey>, next: Option<GlyphKey>) -> bool {
    cached != next
}

fn invalidate_glyph_cache<T>(
    glyph_buffers: &mut Vec<T>,
    glyph_keys: &mut Vec<Option<GlyphKey>>,
    glyph_positions: &mut Vec<(usize, usize)>,
) {
    glyph_buffers.clear();
    glyph_keys.clear();
    glyph_positions.clear();
}
impl TerminalWindow {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let now = Instant::now();
        Self {
            window: None,
            gpu: None,
            tab: None,
            panes: BTreeMap::new(),
            failed: false,
            modifiers: winit::keyboard::ModifiersState::empty(),
            ime_gate: ImePreeditGate::default(),
            surface_retry: SurfaceRetryBackoff::default(),
            proxy,
            exit_poll: ExitPollBackoff::new(now),
            next_exit_poll: now,
            retry_redraw_at: None,
            occluded: false,
            redraw_pending: false,
            cursor_position: PhysicalPosition::new(0.0, 0.0),
            clipboard: None,
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
        let (physical_font_size, physical_cell_width, physical_cell_height) =
            project_terminal_metrics(font_size, cell_width, cell_height, window.scale_factor());
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
        let vertex_buffer_capacity_bytes = 1024;
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terminal reusable cell vertex buffer"),
            size: vertex_buffer_capacity_bytes as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let tab = Tab::from_tree(LayoutTree::new(
            Rect::new(0, 0, width, height),
            CellSize::new(
                physical_cell_width.ceil() as u32,
                physical_cell_height.ceil() as u32,
            ),
            4,
            20,
            5,
        ));
        let pane_id = tab.active_pane();
        let (cols, rows) = tab
            .layout()
            .into_iter()
            .find(|layout| layout.pane == pane_id && !layout.hidden)
            .map(|layout| (layout.cols as usize, layout.rows as usize))
            .unwrap_or_else(|| grid_size(width, height, physical_cell_width, physical_cell_height));
        self.window = Some(Arc::clone(&window));
        self.tab = Some(tab);
        self.panes.insert(
            pane_id,
            Self::create_pane(
                pane_id,
                cols,
                rows,
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                self.proxy.clone(),
            )?,
        );
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
            font_size: physical_font_size,
            logical_font_size: font_size,
            logical_cell_width: cell_width,
            logical_cell_height: cell_height,
            rect_pipeline,
            vertices: Vec::new(),
            vertex_buffer,
            vertex_buffer_capacity_bytes,
        });
        Ok(())
    }
    fn create_pane(
        pane_id: PaneId,
        cols: usize,
        rows: usize,
        cwd: PathBuf,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Pane, String> {
        let wakeup = Arc::new(WakeupGate::default());
        let notify_wakeup = Arc::clone(&wakeup);
        let pty = so1_terminal::pty::Pty::spawn_shell_notified(
            cwd,
            cols as u16,
            rows as u16,
            move || {
                if notify_wakeup.request() {
                    let _ = proxy.send_event(UserEvent::PtyOutput(pane_id));
                }
            },
        )
        .map_err(|error| format!("could not start shell PTY for pane {}: {error}", pane_id.0))?;
        Ok(Pane {
            pty,
            model: Model::new(cols, rows),
            wakeup,
            wheel: WheelAccumulator::default(),
            selection_router: SelectionRouter::default(),
            glyph_buffers: Vec::new(),
            glyph_keys: Vec::new(),
            glyph_positions: Vec::new(),
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        let (w, h) = clamp_surface_size(width, height);
        gpu.config.width = w;
        gpu.config.height = h;
        gpu.surface.configure(&gpu.device, &gpu.config);
        let Some(tab) = self.tab.as_mut() else { return };
        tab.layout_tree_mut().resize_window(Rect::new(0, 0, w, h));
        tab.layout_tree_mut().resize_cell_size(CellSize::new(
            gpu.cell_width.ceil() as u32,
            gpu.cell_height.ceil() as u32,
        ));
        for layout in tab.layout() {
            if layout.hidden {
                continue;
            }
            if let Some(pane) = self.panes.get_mut(&layout.pane) {
                pane.model
                    .resize(layout.cols as usize, layout.rows as usize);
                let _ = pane.pty.resize(
                    layout.cols.min(u16::MAX as u32) as u16,
                    layout.rows.min(u16::MAX as u32) as u16,
                    layout.rect.width.min(u16::MAX as u32) as u16,
                    layout.rect.height.min(u16::MAX as u32) as u16,
                );
                invalidate_glyph_cache(
                    &mut pane.glyph_buffers,
                    &mut pane.glyph_keys,
                    &mut pane.glyph_positions,
                );
            }
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

    fn focused_pane_id(&self) -> Option<PaneId> {
        self.tab.as_ref().map(Tab::active_pane)
    }

    fn focused_pane(&self) -> Option<&Pane> {
        self.panes.get(&self.focused_pane_id()?)
    }

    fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.focused_pane_id()?;
        self.panes.get_mut(&id)
    }

    fn poll_child_exit(&mut self, event_loop: &ActiveEventLoop) {
        let exited: Vec<_> = self
            .panes
            .iter_mut()
            .filter_map(|(id, pane)| pane.pty.try_wait().ok().flatten().map(|_| *id))
            .collect();
        for pane in exited {
            if let Some(tab) = self.tab.as_mut()
                && matches!(
                    tab.close_pane(pane),
                    so1_terminal::layout::CloseResult::LastPane
                )
            {
                self.panes.remove(&pane);
                event_loop.exit();
                return;
            }
            self.panes.remove(&pane);
        }
        if !self.panes.is_empty()
            && let ExitPollDecision::RetryAt(deadline) =
                self.exit_poll.observe_wait_status(false, Instant::now())
        {
            self.next_exit_poll = deadline;
        }
    }

    fn pane_layouts(&self) -> Vec<PaneLayout> {
        self.tab.as_ref().map_or_else(Vec::new, Tab::layout)
    }

    fn pane_and_cell_at_cursor(&self) -> Option<(PaneId, usize, usize)> {
        let gpu = self.gpu.as_ref()?;
        pane_cell_at(
            self.cursor_position.x,
            self.cursor_position.y,
            &self.pane_layouts(),
            gpu.cell_width,
            gpu.cell_height,
        )
    }

    fn split_focused(&mut self, direction: Direction) -> Result<(), String> {
        let pane_id = self
            .tab
            .as_mut()
            .ok_or_else(|| "pane layout is not initialized".to_owned())?
            .split_focused(direction)
            .map_err(|error| format!("cannot split pane: {error:?}"))?;
        let layout = self
            .pane_layouts()
            .into_iter()
            .find(|layout| layout.pane == pane_id && !layout.hidden)
            .ok_or_else(|| "new pane does not satisfy the minimum size".to_owned())?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match Self::create_pane(
            pane_id,
            layout.cols as usize,
            layout.rows as usize,
            cwd,
            self.proxy.clone(),
        ) {
            Ok(pane) => {
                self.panes.insert(pane_id, pane);
                self.resize(
                    self.gpu.as_ref().map_or(1, |gpu| gpu.config.width),
                    self.gpu.as_ref().map_or(1, |gpu| gpu.config.height),
                );
                self.request_redraw();
                Ok(())
            }
            Err(error) => {
                if let Some(tab) = self.tab.as_mut() {
                    let _ = tab.close_pane(pane_id);
                }
                Err(error)
            }
        }
    }

    fn close_focused(&mut self, event_loop: &ActiveEventLoop) {
        let Some(tab) = self.tab.as_mut() else { return };
        let pane_id = tab.active_pane();
        if matches!(
            tab.close_pane(pane_id),
            so1_terminal::layout::CloseResult::LastPane
        ) {
            self.panes.remove(&pane_id);
            event_loop.exit();
            return;
        }
        self.panes.remove(&pane_id);
        if let Some(gpu) = self.gpu.as_ref() {
            self.resize(gpu.config.width, gpu.config.height);
        }
        self.request_redraw();
    }

    fn focus_direction(&mut self, direction: FocusDirection) {
        if let Some(tab) = self.tab.as_mut() {
            tab.move_focus(direction);
        }
        self.request_redraw();
    }

    fn clipboard(&mut self) -> Option<&mut Clipboard> {
        if self.clipboard.is_none() {
            match Clipboard::new() {
                Ok(clipboard) => self.clipboard = Some(clipboard),
                Err(error) => {
                    eprintln!("so1term: clipboard initialization failed: {error}");
                    return None;
                }
            }
        }
        self.clipboard.as_mut()
    }

    fn copy_selection(&mut self) {
        let Some(text) = self
            .focused_pane()
            .and_then(|pane| pane.model.selection_text())
        else {
            return;
        };
        let Some(clipboard) = self.clipboard() else {
            return;
        };
        if let Err(error) = clipboard.set_text(text) {
            eprintln!("so1term: clipboard copy failed: {error}");
        }
    }

    fn copy_block_text(&mut self, input: bool) {
        let text = self.focused_pane().and_then(|pane| {
            if input {
                pane.model.selected_block_input_text()
            } else {
                pane.model.selected_block_output_text()
            }
        });
        let Some(text) = text else { return };
        let Some(clipboard) = self.clipboard() else {
            return;
        };
        if let Err(error) = clipboard.set_text(text) {
            eprintln!("so1term: clipboard copy failed: {error}");
        }
    }

    fn handle_block_shortcut(&mut self, action: BlockShortcut) {
        match action {
            BlockShortcut::Previous => {
                if let Some(pane) = self.focused_pane_mut() {
                    pane.model.navigate_block(
                        so1_terminal::block_navigation::NavigationDirection::Previous,
                    );
                }
            }
            BlockShortcut::Next => {
                if let Some(pane) = self.focused_pane_mut() {
                    pane.model
                        .navigate_block(so1_terminal::block_navigation::NavigationDirection::Next);
                }
            }
            BlockShortcut::Select => {
                if let Some(pane) = self.focused_pane_mut() {
                    pane.model.select_block_at_viewport();
                }
            }
            BlockShortcut::CopyOutput => self.copy_block_text(false),
            BlockShortcut::CopyInput => self.copy_block_text(true),
        }
        self.request_redraw();
    }

    fn paste_clipboard(&mut self) {
        let text = match self
            .clipboard()
            .and_then(|clipboard| match clipboard.get_text() {
                Ok(text) => Some(text),
                Err(error) => {
                    eprintln!("so1term: clipboard paste failed: {error}");
                    None
                }
            }) {
            Some(text) => text,
            None => return,
        };
        let bracketed = self
            .focused_pane()
            .is_some_and(|pane| pane.model.bracketed_paste_enabled());
        let bytes = encode_paste(&text, bracketed);
        if let Some(pane) = self.focused_pane_mut()
            && let Err(error) = pane.pty.write(&bytes)
        {
            eprintln!("so1term: clipboard paste write failed: {error}");
        }
        if let Some(pane) = self.focused_pane_mut() {
            pane.model.scroll_to_bottom();
        }
        self.request_redraw();
    }

    fn draw(&mut self) -> Result<FrameRecovery, String> {
        let layouts = self.pane_layouts();
        let focused = self.focused_pane_id();
        let dividers = self.tab.as_ref().map_or_else(Vec::new, |tab| {
            tab.layout_tree().divider_rects(tab.active_pane())
        });
        let Some(gpu) = self.gpu.as_mut() else {
            return Ok(FrameRecovery::WaitForEvent);
        };
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Lost => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return Ok(FrameRecovery::RetryAfter(self.surface_retry.next_delay()));
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return Ok(FrameRecovery::RetryAfter(self.surface_retry.next_delay()));
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
        self.surface_retry.reset();
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut rendered_any = false;
        let panes = &mut self.panes;
        for layout in layouts.iter().filter(|layout| !layout.hidden) {
            let Some(pane) = panes.get_mut(&layout.pane) else {
                continue;
            };
            let rows = pane.model.visible_cells();
            let cursor = pane.model.cursor();
            let mut cells =
                so1_terminal::renderer::render_cells(&rows, (cursor.column, cursor.row));
            for (column, row) in pane.model.selected_cells() {
                if let Some(cell) = cells.get_mut(row).and_then(|row| row.get_mut(column)) {
                    apply_selection_highlight(cell);
                }
            }
            gpu.vertices.clear();
            let width = gpu.config.width as f32;
            let height = gpu.config.height as f32;
            for (row, line) in cells.iter().enumerate() {
                for (column, cell) in line.iter().enumerate() {
                    push_overlay_rect(
                        &mut gpu.vertices,
                        layout.rect.x as f32 + column as f32 * gpu.cell_width,
                        layout.rect.y as f32 + row as f32 * gpu.cell_height,
                        layout.rect.x as f32 + (column + 1) as f32 * gpu.cell_width,
                        layout.rect.y as f32 + (row + 1) as f32 * gpu.cell_height,
                        width,
                        height,
                        cell.background,
                    );
                }
            }
            let background_vertices = gpu.vertices.len();
            let block_viewport = BlockViewport {
                evicted_rows: pane.model.block_row_base(),
                history_size: pane.model.history_size(),
                display_offset: pane.model.display_offset(),
                first_visible_line: Some(pane.model.visible_top_line()),
                screen_lines: rows.len(),
                alternate_screen: pane.model.alternate_screen_active(),
            };
            for decoration in project_block_decorations(pane.model.blocks(), block_viewport) {
                let color = match decoration.color {
                    BlockDecorationColor::Accent => [105, 165, 220],
                    BlockDecorationColor::Success => [80, 200, 120],
                    BlockDecorationColor::Failure => [230, 75, 75],
                    BlockDecorationColor::Muted => [110, 115, 125],
                };
                if let Some(row) = decoration.separator_row {
                    let y = layout.rect.y as f32 + row as f32 * gpu.cell_height;
                    push_overlay_rect(
                        &mut gpu.vertices,
                        layout.rect.x as f32,
                        y,
                        layout.rect.x as f32 + layout.rect.width as f32,
                        y + 1.5,
                        width,
                        height,
                        color,
                    );
                }
                let row_top = layout.rect.y as f32 + decoration.marker_row as f32 * gpu.cell_height;
                let [x0, y0, x1, y1] = block_marker_bounds(gpu.cell_width, gpu.cell_height);
                push_overlay_rect(
                    &mut gpu.vertices,
                    layout.rect.x as f32 + x0,
                    row_top + y0,
                    layout.rect.x as f32 + x1,
                    row_top + y1,
                    width,
                    height,
                    color,
                );
            }
            if focused == Some(layout.pane) {
                let border = [90, 145, 205];
                let x = layout.rect.x as f32;
                let y = layout.rect.y as f32;
                let right = (x + layout.rect.width as f32).min(width);
                let bottom = (y + layout.rect.height as f32).min(height);
                push_overlay_rect(
                    &mut gpu.vertices,
                    x,
                    y,
                    right,
                    y + 1.0,
                    width,
                    height,
                    border,
                );
                push_overlay_rect(
                    &mut gpu.vertices,
                    x,
                    bottom - 1.0,
                    right,
                    bottom,
                    width,
                    height,
                    border,
                );
                push_overlay_rect(
                    &mut gpu.vertices,
                    x,
                    y,
                    x + 1.0,
                    bottom,
                    width,
                    height,
                    border,
                );
                push_overlay_rect(
                    &mut gpu.vertices,
                    right - 1.0,
                    y,
                    right,
                    bottom,
                    width,
                    height,
                    border,
                );
            }
            collect_glyph_positions(&cells, &mut pane.glyph_positions);
            pane.glyph_keys.resize(pane.glyph_positions.len(), None);
            for (index, &(row, column)) in pane.glyph_positions.iter().enumerate() {
                let cell = cells[row][column];
                if index == pane.glyph_buffers.len() {
                    pane.glyph_buffers.push(Buffer::new(
                        &mut gpu.font_system,
                        Metrics::new(gpu.font_size, gpu.cell_height),
                    ));
                }
                let width_cells = unicode_cell_width(cell.character).max(1);
                let key = (cell.character, cell.foreground, cell.bold, width_cells);
                if !glyph_key_requires_reshape(pane.glyph_keys[index], Some(key)) {
                    continue;
                }
                let buffer = &mut pane.glyph_buffers[index];
                buffer.set_size(
                    Some(gpu.cell_width * width_cells as f32),
                    Some(gpu.cell_height),
                );
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
                let mut text = [0; 4];
                buffer.set_text(
                    cell.character.encode_utf8(&mut text),
                    &attrs,
                    cosmic_text::Shaping::Advanced,
                    None,
                );
                buffer.shape_until_scroll(&mut gpu.font_system, true);
                pane.glyph_keys[index] = Some(key);
            }
            gpu.viewport.update(
                &gpu.queue,
                Resolution {
                    width: gpu.config.width,
                    height: gpu.config.height,
                },
            );
            let areas: Vec<_> = pane
                .glyph_positions
                .iter()
                .enumerate()
                .map(|(index, &(row, column))| TextArea {
                    buffer: &pane.glyph_buffers[index],
                    left: layout.rect.x as f32 + column as f32 * gpu.cell_width,
                    top: layout.rect.y as f32 + row as f32 * gpu.cell_height,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: layout.rect.x as i32,
                        top: layout.rect.y as i32,
                        right: layout.rect.right().min(gpu.config.width) as i32,
                        bottom: layout.rect.bottom().min(gpu.config.height) as i32,
                    },
                    default_color: Color::rgb(220, 220, 220),
                    custom_glyphs: &[],
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
                .map_err(|error| format!("could not prepare pane text: {error}"))?;
            let required = gpu.vertices.len() * std::mem::size_of::<RectVertex>();
            let capacity = so1_terminal::renderer::next_vertex_buffer_capacity_bytes(
                gpu.vertex_buffer_capacity_bytes,
                required,
            );
            if capacity > gpu.vertex_buffer_capacity_bytes {
                gpu.vertex_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("terminal reusable cell vertex buffer"),
                    size: capacity as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                gpu.vertex_buffer_capacity_bytes = capacity;
            }
            if required > 0 {
                gpu.queue
                    .write_buffer(&gpu.vertex_buffer, 0, bytemuck::cast_slice(&gpu.vertices));
            }
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("so1term pane renderer"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("terminal pane cells and text"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: if rendered_any {
                                wgpu::LoadOp::Load
                            } else {
                                wgpu::LoadOp::Clear(wgpu::Color {
                                    r: so1_terminal::renderer::surface_clear_color()[0],
                                    g: so1_terminal::renderer::surface_clear_color()[1],
                                    b: so1_terminal::renderer::surface_clear_color()[2],
                                    a: so1_terminal::renderer::surface_clear_color()[3],
                                })
                            },
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_scissor_rect(
                    layout.rect.x,
                    layout.rect.y,
                    layout
                        .rect
                        .width
                        .min(gpu.config.width.saturating_sub(layout.rect.x)),
                    layout
                        .rect
                        .height
                        .min(gpu.config.height.saturating_sub(layout.rect.y)),
                );
                if background_vertices > 0 {
                    pass.set_pipeline(&gpu.rect_pipeline);
                    pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
                    pass.draw(0..background_vertices as u32, 0..1);
                }
                gpu.text_renderer
                    .render(&gpu.atlas, &gpu.viewport, &mut pass)
                    .map_err(|error| format!("could not render pane text: {error}"))?;
                if let Some(range) = overlay_vertex_range(background_vertices, gpu.vertices.len()) {
                    pass.set_pipeline(&gpu.rect_pipeline);
                    pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
                    pass.draw(range, 0..1);
                }
            }
            gpu.queue.submit([encoder.finish()]);
            rendered_any = true;
        }
        if !rendered_any {
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("so1term empty surface"),
                });
            {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("clear surface with no visible panes"),
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
            }
            gpu.queue.submit([encoder.finish()]);
        }
        if !dividers.is_empty() {
            gpu.vertices.clear();
            for rect in &dividers {
                push_overlay_rect(
                    &mut gpu.vertices,
                    rect.x as f32,
                    rect.y as f32,
                    rect.right().min(gpu.config.width) as f32,
                    rect.bottom().min(gpu.config.height) as f32,
                    gpu.config.width as f32,
                    gpu.config.height as f32,
                    [48, 53, 62],
                );
            }
            let required = gpu.vertices.len() * std::mem::size_of::<RectVertex>();
            if required > gpu.vertex_buffer_capacity_bytes {
                let capacity = so1_terminal::renderer::next_vertex_buffer_capacity_bytes(
                    gpu.vertex_buffer_capacity_bytes,
                    required,
                );
                gpu.vertex_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("terminal divider vertex buffer"),
                    size: capacity as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                gpu.vertex_buffer_capacity_bytes = capacity;
            }
            gpu.queue
                .write_buffer(&gpu.vertex_buffer, 0, bytemuck::cast_slice(&gpu.vertices));
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("so1term pane dividers"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("pane divider overlay"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&gpu.rect_pipeline);
                pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
                for (index, rect) in dividers.iter().enumerate() {
                    pass.set_scissor_rect(
                        rect.x,
                        rect.y,
                        rect.width.min(gpu.config.width.saturating_sub(rect.x)),
                        rect.height.min(gpu.config.height.saturating_sub(rect.y)),
                    );
                    let start = (index * 6) as u32;
                    pass.draw(start..start + 6, 0..1);
                }
            }
            gpu.queue.submit([encoder.finish()]);
        }
        gpu.queue.present(frame);
        Ok(FrameRecovery::WaitForEvent)
    }
    fn receive_output(&mut self, pane_id: PaneId, event_loop: &ActiveEventLoop) {
        let mut changed = false;
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            for bytes in pane.pty.drain_output() {
                changed = true;
                pane.model.feed(&bytes);
                for ev in pane.model.take_events() {
                    match ev {
                        ModelEvent::PtyWrite(bytes) => {
                            let _ = pane.pty.write(&bytes);
                        }
                        ModelEvent::Title(title) => {
                            if let Some(window) = &self.window {
                                window.set_title(&format!("so1term — {title}"));
                            }
                        }
                        ModelEvent::ResetTitle => {
                            if let Some(window) = &self.window {
                                window.set_title("so1term");
                            }
                        }
                        ModelEvent::Bell | ModelEvent::Other(_) => {}
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
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let UserEvent::PtyOutput(pane_id) = event;
        if let Some(pane) = self.panes.get(&pane_id) {
            pane.wakeup.acknowledge();
        }
        self.receive_output(pane_id, event_loop)
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.panes.clear();
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
                self.ime_gate.preedit(&preedit);
            }
            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                if let Some(pane) = self.focused_pane_mut() {
                    let bytes = encode_ime_commit(&text, pane.model.bracketed_paste_enabled());
                    let _ = pane.pty.write(&bytes);
                    pane.model.scroll_to_bottom();
                }
                self.request_redraw();
                self.ime_gate.commit();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let mods = KeyModifiers {
                    shift: self.modifiers.contains(ModifiersState::SHIFT),
                    alt: self.modifiers.contains(ModifiersState::ALT),
                    control: self.modifiers.contains(ModifiersState::CONTROL),
                    super_key: self.modifiers.contains(ModifiersState::SUPER),
                };
                let action = block_shortcut(&event.logical_key, mods, cfg!(target_os = "macos"));
                let action = route_block_shortcut(
                    action,
                    self.focused_pane()
                        .is_some_and(|pane| pane.model.alternate_screen_active()),
                );
                if let Some(action) = action {
                    self.handle_block_shortcut(action);
                    return;
                }
                match pane_action(&event.logical_key, mods, cfg!(target_os = "macos")) {
                    Some(PaneAction::SplitRight) => {
                        if let Err(error) = self.split_focused(Direction::Horizontal) {
                            eprintln!("so1term: {error}");
                        }
                        return;
                    }
                    Some(PaneAction::SplitDown) => {
                        if let Err(error) = self.split_focused(Direction::Vertical) {
                            eprintln!("so1term: {error}");
                        }
                        return;
                    }
                    Some(PaneAction::Close) => {
                        self.close_focused(event_loop);
                        return;
                    }
                    Some(PaneAction::Focus(direction)) => {
                        self.focus_direction(direction);
                        return;
                    }
                    None => {}
                }
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
                if let Some(TerminalKey::Text(text)) = terminal_key {
                    let clipboard_shortcut = (mods.control && mods.shift)
                        || (cfg!(target_os = "macos") && mods.super_key);
                    if clipboard_shortcut && text.eq_ignore_ascii_case("c") {
                        self.copy_selection();
                        return;
                    }
                    if clipboard_shortcut && text.eq_ignore_ascii_case("v") {
                        self.paste_clipboard();
                        return;
                    }
                }
                let suppress_key =
                    terminal_key.is_some_and(|key| self.ime_gate.suppresses_key(key, mods.control));
                if let (Some(key), Some(pane)) = (terminal_key, self.focused_pane_mut())
                    && !suppress_key
                {
                    let app_cursor = pane.model.application_cursor_keys();
                    if let Some(bytes) = encode_key(key, mods, app_cursor) {
                        let _ = pane.pty.write(&bytes);
                    }
                    pane.model.scroll_to_bottom();
                }
                self.request_redraw();
            }
            WindowEvent::Ime(winit::event::Ime::Enabled) => {}
            WindowEvent::Ime(winit::event::Ime::Disabled) => self.ime_gate.disable(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = position;
                if let Some((pane_id, column, row)) = self.pane_and_cell_at_cursor()
                    && let Some(pane) = self.panes.get_mut(&pane_id)
                    && pane.selection_router.cursor_moved(Some((column, row)))
                        == PointerAction::Extend
                {
                    pane.model.extend_selection(column, row);
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = if button == MouseButton::Left {
                    PointerButton::Primary
                } else {
                    PointerButton::Other
                };
                let state = if state == ElementState::Pressed {
                    PointerState::Pressed
                } else {
                    PointerState::Released
                };
                let hit = self.pane_and_cell_at_cursor();
                if state == PointerState::Pressed
                    && let Some((pane_id, _, _)) = hit
                    && let Some(tab) = self.tab.as_mut()
                {
                    let _ = tab.focus_by_click(
                        self.cursor_position.x.max(0.0) as u32,
                        self.cursor_position.y.max(0.0) as u32,
                    );
                    debug_assert_eq!(tab.active_pane(), pane_id);
                }
                let action = hit
                    .and_then(|(pane_id, column, row)| {
                        let pane = self.panes.get_mut(&pane_id)?;
                        Some(pane.selection_router.mouse_input(
                            button,
                            state,
                            Some((column, row)),
                            Instant::now(),
                        ))
                    })
                    .unwrap_or(PointerAction::Ignore);
                let pane_id = hit.map(|(pane_id, _, _)| pane_id);
                match action {
                    PointerAction::Begin(gesture) => {
                        if let (Some((_, column, row)), Some(pane)) = (
                            hit,
                            pane_id.and_then(|pane_id| self.panes.get_mut(&pane_id)),
                        ) {
                            let kind = match gesture {
                                SelectionGesture::Simple => SelectionType::Simple,
                                SelectionGesture::Semantic => SelectionType::Semantic,
                                SelectionGesture::Lines => SelectionType::Lines,
                            };
                            pane.model.begin_selection(column, row, kind);
                        }
                    }
                    PointerAction::Clear => {
                        if let Some(pane) = pane_id.and_then(|pane_id| self.panes.get_mut(&pane_id))
                        {
                            pane.model.clear_selection();
                        }
                    }
                    _ => {}
                }
                if action != PointerAction::Ignore {
                    self.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some((pane_id, _, _)) = self.pane_and_cell_at_cursor()
                    && let Some(pane) = self.panes.get_mut(&pane_id)
                    && !pane.model.mouse_or_alternate_screen_active()
                {
                    let line_delta = match delta {
                        MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                        MouseScrollDelta::PixelDelta(pos) => pos.y / 16.0,
                    };
                    let lines = pane.wheel.push_lines(line_delta);
                    if lines > 0 {
                        pane.model.scroll_up(lines as usize);
                    }
                    if lines < 0 {
                        pane.model.scroll_down(lines.unsigned_abs() as usize);
                    }
                    if lines != 0 {
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(gpu) = self.gpu.as_mut() {
                    (gpu.font_size, gpu.cell_width, gpu.cell_height) = project_terminal_metrics(
                        gpu.logical_font_size,
                        gpu.logical_cell_width,
                        gpu.logical_cell_height,
                        scale_factor,
                    );
                    // Buffers retain their Metrics, so discard them when the device scale changes.
                    for pane in self.panes.values_mut() {
                        invalidate_glyph_cache(
                            &mut pane.glyph_buffers,
                            &mut pane.glyph_keys,
                            &mut pane.glyph_positions,
                        );
                    }
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
        if !self.panes.is_empty() && now >= self.next_exit_poll {
            self.poll_child_exit(event_loop);
        }
        if self.retry_redraw_at.is_some_and(|deadline| deadline <= now) {
            self.retry_redraw_at = None;
            self.request_redraw();
        }
        let next_deadline = [
            (!self.panes.is_empty()).then_some(self.next_exit_poll),
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

#[cfg(test)]
mod tests {
    use super::{
        BlockShortcut, GlyphKey, PaneAction, block_marker_bounds, block_shortcut,
        glyph_key_requires_reshape, invalidate_glyph_cache, overlay_vertex_range, pane_action,
        pane_cell_at, route_block_shortcut,
    };
    use so1_terminal::keyboard::Modifiers;
    use so1_terminal::layout::{PaneId, PaneLayout, Rect};
    use so1_terminal::model::Model;
    use winit::keyboard::{Key, NamedKey};

    #[test]
    fn pointer_routing_uses_visible_pane_rects_and_local_cell_coordinates() {
        let panes = [
            PaneLayout {
                pane: PaneId(101),
                rect: Rect::new(0, 0, 100, 80),
                cols: 10,
                rows: 8,
                hidden: false,
            },
            PaneLayout {
                pane: PaneId(102),
                rect: Rect::new(104, 0, 96, 80),
                cols: 9,
                rows: 8,
                hidden: false,
            },
            PaneLayout {
                pane: PaneId(103),
                rect: Rect::new(0, 0, 0, 0),
                cols: 0,
                rows: 0,
                hidden: true,
            },
        ];
        let cases = [
            (25.0, 35.0, Some((PaneId(101), 2, 3))),
            (124.0, 75.0, Some((PaneId(102), 2, 7))),
            (102.0, 20.0, None),
            (210.0, 20.0, None),
        ];
        for (x, y, expected) in cases {
            assert_eq!(pane_cell_at(x, y, &panes, 10.0, 10.0), expected);
        }
    }

    #[test]
    fn pane_shortcuts_are_distinct_from_terminal_and_block_navigation_keys() {
        let control_shift = Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        };
        let control_alt = Modifiers {
            control: true,
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            pane_action(&Key::Character("d".into()), control_shift, false),
            Some(PaneAction::SplitRight)
        );
        assert_eq!(
            pane_action(&Key::Character("e".into()), control_shift, false),
            Some(PaneAction::SplitDown)
        );
        assert_eq!(
            pane_action(&Key::Character("w".into()), control_shift, false),
            Some(PaneAction::Close)
        );
        assert_eq!(
            pane_action(&Key::Named(NamedKey::ArrowLeft), control_alt, false),
            Some(PaneAction::Focus(
                so1_terminal::layout::FocusDirection::Left
            ))
        );
        assert_eq!(
            pane_action(&Key::Named(NamedKey::ArrowUp), control_shift, false),
            None
        );

        let command_shift = Modifiers {
            super_key: true,
            shift: true,
            ..Modifiers::default()
        };
        let command_alt = Modifiers {
            super_key: true,
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            pane_action(&Key::Character("D".into()), command_shift, true),
            Some(PaneAction::SplitRight)
        );
        assert_eq!(
            pane_action(&Key::Character("W".into()), command_shift, true),
            Some(PaneAction::Close)
        );
        assert_eq!(
            pane_action(&Key::Named(NamedKey::ArrowDown), command_alt, true),
            Some(PaneAction::Focus(
                so1_terminal::layout::FocusDirection::Down
            ))
        );
    }

    #[test]
    #[cfg(unix)]
    fn two_headless_pty_models_keep_output_isolated() {
        use std::{
            path::PathBuf,
            time::{Duration, Instant},
        };

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let first_mark = "PANE_ALPHA_MARK";
        let second_mark = "PANE_BETA_MARK";
        let first_pty = so1_terminal::pty::Pty::spawn_with_args(
            "/bin/sh",
            ["-c", "printf PANE_ALPHA_MARK; sleep 0.1"],
            &cwd,
            80,
            24,
        )
        .expect("spawn first PTY");
        let second_pty = so1_terminal::pty::Pty::spawn_with_args(
            "/bin/sh",
            ["-c", "printf PANE_BETA_MARK; sleep 0.1"],
            &cwd,
            80,
            24,
        )
        .expect("spawn second PTY");
        let mut first_model = Model::new(80, 24);
        let mut second_model = Model::new(80, 24);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            for bytes in first_pty.drain_output() {
                first_model.feed(&bytes);
            }
            for bytes in second_pty.drain_output() {
                second_model.feed(&bytes);
            }
            let first_text: String = first_model
                .visible_cells()
                .iter()
                .flatten()
                .map(|cell| cell.character)
                .collect();
            let second_text: String = second_model
                .visible_cells()
                .iter()
                .flatten()
                .map(|cell| cell.character)
                .collect();
            if first_text.contains(first_mark) && second_text.contains(second_mark) {
                assert!(!first_text.contains(second_mark));
                assert!(!second_text.contains(first_mark));
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("PTY output markers were not observed before the deadline");
    }

    #[test]
    fn block_shortcuts_are_distinct_from_copy_and_paste() {
        let control_shift = Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            block_shortcut(&Key::Named(NamedKey::ArrowUp), control_shift, false),
            Some(BlockShortcut::Previous)
        );
        assert_eq!(
            block_shortcut(&Key::Named(NamedKey::ArrowDown), control_shift, false),
            Some(BlockShortcut::Next)
        );
        assert_eq!(
            block_shortcut(&Key::Character("a".into()), control_shift, false),
            Some(BlockShortcut::Select)
        );
        assert_eq!(
            block_shortcut(&Key::Character("o".into()), control_shift, false),
            Some(BlockShortcut::CopyOutput)
        );
        assert_eq!(
            block_shortcut(&Key::Character("l".into()), control_shift, false),
            Some(BlockShortcut::CopyInput)
        );
        assert_eq!(
            block_shortcut(&Key::Character("c".into()), control_shift, false),
            None
        );
        assert_eq!(
            block_shortcut(&Key::Character("v".into()), control_shift, false),
            None
        );
        let command_shift = Modifiers {
            super_key: true,
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            block_shortcut(&Key::Character("a".into()), command_shift, true),
            Some(BlockShortcut::Select)
        );
        let command = Modifiers {
            super_key: true,
            ..Modifiers::default()
        };
        assert_eq!(
            block_shortcut(&Key::Named(NamedKey::ArrowUp), command, true),
            Some(BlockShortcut::Previous)
        );
        assert_eq!(
            block_shortcut(&Key::Named(NamedKey::ArrowDown), command, true),
            Some(BlockShortcut::Next)
        );
        assert_eq!(
            block_shortcut(&Key::Character("o".into()), command_shift, true),
            None
        );
    }

    #[test]
    fn block_shortcuts_are_routed_to_the_application_in_alternate_screen() {
        assert_eq!(
            route_block_shortcut(Some(BlockShortcut::Previous), true),
            None
        );
        assert_eq!(
            route_block_shortcut(Some(BlockShortcut::Previous), false),
            Some(BlockShortcut::Previous)
        );
        assert_eq!(route_block_shortcut(None, true), None);
    }

    #[test]
    fn decoration_vertices_are_separate_for_compositing_after_text() {
        assert_eq!(overlay_vertex_range(12, 24), Some(12..24));
        assert_eq!(overlay_vertex_range(12, 12), None);
    }

    #[test]
    fn block_marker_uses_the_first_cell_trailing_padding() {
        assert_eq!(block_marker_bounds(8.0, 20.0), [5.5, 2.0, 8.0, 18.0]);
    }

    #[test]
    fn glyph_cache_reshapes_only_when_a_visible_glyph_changes() {
        let key = Some(('日', [255, 0, 0], true, 2));
        assert!(glyph_key_requires_reshape(None, key));
        assert!(!glyph_key_requires_reshape(key, key));
        assert!(glyph_key_requires_reshape(
            key,
            Some(('日', [0, 255, 0], true, 2))
        ));
    }

    #[test]
    fn scale_change_invalidates_buffers_with_stale_font_metrics() {
        let mut buffers = vec![7_u8, 8];
        let mut keys: Vec<Option<GlyphKey>> = vec![Some(('x', [1, 2, 3], false, 1))];
        let mut positions = vec![(0, 0), (1, 0)];

        invalidate_glyph_cache(&mut buffers, &mut keys, &mut positions);

        assert!(buffers.is_empty());
        assert!(keys.is_empty());
        assert!(positions.is_empty());
    }
}
