mod cards;
mod chrome;
mod help;

use chrome::UiMode;
use help::HelpRoute;
use nulix::blocks::PaneActivity;

use alacritty_terminal::selection::SelectionType;
use arboard::Clipboard;
use cosmic_text::{Attrs, Buffer, Family, Metrics, Weight};
use glyphon::{
    Cache, Color, FontSystem, Resolution, SwashCache, TextArea, TextAtlas, TextBounds,
    TextRenderer, Viewport,
};
use graphics::{DecodeJob, DecodedImage, GraphicsCompletion, GraphicsStore};
use nulix::control::{
    self as control, ControlCommand, ControlError, ControlErrorCode, ControlOutcome,
    ControlRequest, ControlResponse, ControlServer, TextScope,
};
use nulix::{
    PaneRenderInput,
    attach::{
        BlockOp, ClientMessage, CopySource, FocusDirection as RemoteFocusDirection, LayoutAction,
        SelectionOp, ServerMessage, SplitDirection,
    },
    clamp_surface_size, graphics,
    graphics_pipeline::{GraphicsHost, PaneGraphics, SharedGraphics, frame_placements},
    keyboard::{
        ImePreeditGate, Key as TerminalKey, KeyEventKind, KeyInput, KeyboardFlags,
        Modifiers as KeyModifiers, encode_ime_commit_with_flags, encode_key_with_flags,
        encode_paste,
    },
    layout::{CellSize, Direction, FocusDirection, PaneId, PaneLayout, Rect, Tab, TabSet},
    model::{Model, ModelEvent},
    remote::{
        AttachConnection, AttachSettle, FrameOutcome, ImageChange, RemoteEvent, RemoteView,
        ViewChange, WindowMetrics,
    },
    renderer::{
        BlockDecorationColor, ExitPollBackoff, ExitPollDecision, FrameRecovery, SurfaceFailure,
        SurfaceRetryBackoff, WakeupGate, WheelAccumulator, collect_glyph_positions, grid_size,
        project_terminal_metrics, unicode_cell_width,
    },
    selection::{PointerAction, PointerButton, PointerState, SelectionGesture, SelectionRouter},
    session::{
        apply_title_event, close_pane_lifecycle, create_tab_with, cwd_basename_title,
        pane_tab_title,
    },
};
use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsString,
    path::PathBuf,
    sync::Arc,
    time::Instant,
};
use winit::dpi::PhysicalPosition;
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowAttributes, WindowId},
};

enum UserEvent {
    PtyOutput(PaneId),
    GraphicsDecoded(PaneId, nulix::blocks::GridAnchor, GraphicsCompletion),
    Control(ControlRequest, std::sync::mpsc::SyncSender<ControlResponse>),
    Remote(RemoteEvent),
}
struct TerminalWindow {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    tabs: Option<TabSet>,
    panes: BTreeMap<PaneId, Pane>,
    selection_origin: Option<PaneId>,
    failed: bool,
    modifiers: winit::keyboard::ModifiersState,
    caps_lock: bool,
    num_lock: bool,
    consumed_keys: ConsumedKeyPresses,
    ime_gate: ImePreeditGate,
    surface_retry: SurfaceRetryBackoff,
    proxy: EventLoopProxy<UserEvent>,
    exit_poll: ExitPollBackoff,
    next_exit_poll: Instant,
    retry_redraw_at: Option<Instant>,
    /// Up mode: next redraw that advances running cards' elapsed time.
    running_tick_at: Option<Instant>,
    occluded: bool,
    redraw_pending: bool,
    cursor_position: PhysicalPosition<f64>,
    clipboard: Option<Clipboard>,
    /// Process-wide kitty image storage of the standalone window's panes.
    graphics: SharedGraphics,
    _control_server: Option<ControlServer>,
    /// Attached-client state; `None` for the standalone window.
    remote: Option<RemoteClient>,
    /// Chrome of the window: classic, or `nulix up`'s bars (standalone only).
    ui_mode: UiMode,
    /// Up mode: the F1 help overlay is shown (see `help.rs`).
    help_open: bool,
}

/// Window state of `nulix --session NAME`: the server owns PTYs and models,
/// so this holds only the attach stream, the frame store, and per-pane
/// pointer/glyph state that stays local to the window.
struct RemoteClient {
    socket: PathBuf,
    connection: Option<AttachConnection>,
    view: RemoteView,
    panes: BTreeMap<PaneId, RemotePaneState>,
    next_request: u64,
    copy_requests: HashSet<u64>,
    /// Holds the attach back until the window size settled (see [`AttachSettle`]).
    settle: AttachSettle,
    /// Set once the window detaches or the server reports `Exit`, so the
    /// following end of stream is not reported as a broken connection.
    closing: bool,
}

#[derive(Default)]
struct RemotePaneState {
    glyphs: GlyphCache,
    selection_router: SelectionRouter,
    wheel: WheelAccumulator,
}

impl RemoteClient {
    fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            connection: None,
            view: RemoteView::default(),
            panes: BTreeMap::new(),
            next_request: 1,
            copy_requests: HashSet::new(),
            settle: AttachSettle::new(Instant::now()),
            closing: false,
        }
    }

    /// Attaches with the settled window metrics; `Hello` is sent by the connection.
    fn attach(
        &mut self,
        metrics: WindowMetrics,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<(), String> {
        self.settle.attached();
        let connection = AttachConnection::attach(&self.socket, metrics, move |event| {
            let _ = proxy.send_event(UserEvent::Remote(event));
        })
        .map_err(|error| {
            format!(
                "could not attach to session socket {}: {error}",
                self.socket.display()
            )
        })?;
        self.connection = Some(connection);
        Ok(())
    }

    /// Send failures are not reported here: the reader thread observes the
    /// same broken stream and closes the window with one diagnostic.
    fn send(&self, message: ClientMessage) {
        if let Some(connection) = &self.connection {
            let _ = connection.send(&message);
        }
    }

    fn request_id(&mut self) -> u64 {
        let id = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        id
    }

    fn layout(&mut self, action: LayoutAction) {
        let request = self.request_id();
        self.send(ClientMessage::Layout { request, action });
    }

    fn copy(&mut self, pane: PaneId, source: CopySource) {
        let request = self.request_id();
        self.copy_requests.insert(request);
        self.send(ClientMessage::Copy {
            request,
            pane: pane.0,
            source,
        });
    }

    fn pane_state(&mut self, pane: PaneId) -> &mut RemotePaneState {
        self.panes.entry(pane).or_default()
    }
}

/// Input modes of the focused pane, from its local model or its latest frame.
#[derive(Clone, Copy)]
struct FocusedModes {
    keyboard_flags: KeyboardFlags,
    app_cursor: bool,
    bracketed_paste: bool,
    alternate_screen: bool,
}

/// Shaped glyph buffers for one pane, reused across frames.
#[derive(Default)]
struct GlyphCache {
    buffers: Vec<Buffer>,
    keys: Vec<Option<GlyphKey>>,
    positions: Vec<(usize, usize)>,
}

impl GlyphCache {
    fn invalidate(&mut self) {
        invalidate_glyph_cache(&mut self.buffers, &mut self.keys, &mut self.positions);
    }
}

#[derive(Default)]
struct ConsumedKeyPresses(HashSet<winit::keyboard::PhysicalKey>);

impl ConsumedKeyPresses {
    fn mark(&mut self, key: winit::keyboard::PhysicalKey) {
        self.0.insert(key);
    }

    fn suppress_press(&self, key: winit::keyboard::PhysicalKey) -> bool {
        self.0.contains(&key)
    }

    fn suppress_release(&mut self, key: winit::keyboard::PhysicalKey) -> bool {
        self.0.remove(&key)
    }
}
struct Pane {
    pty: nulix::pty::Pty,
    model: Model,
    graphics: PaneGraphics,
    title: String,
    explicit_title: Option<String>,
    cwd: PathBuf,
    wakeup: Arc<WakeupGate>,
    wheel: WheelAccumulator,
    selection_router: SelectionRouter,
    glyphs: GlyphCache,
}

impl Pane {
    /// Runs queued PTY output through the shared graphics pipeline and the
    /// model, writing replies and terminal query answers to the PTY.
    fn apply_output(
        &mut self,
        id: PaneId,
        shared: &mut SharedGraphics,
        host: &mut dyn GraphicsHost,
    ) {
        let Pane {
            pty,
            model,
            graphics,
            title,
            explicit_title,
            cwd,
            ..
        } = self;
        graphics.apply(
            id,
            model,
            shared,
            host,
            &mut |reply| {
                let _ = pty.write(reply);
            },
            &mut |event| {
                if let nulix::osc::Event::CurrentDirectory { path, .. } = event {
                    let text = String::from_utf8_lossy(&path).into_owned();
                    *cwd = PathBuf::from(text);
                    *title = pane_tab_title(cwd, explicit_title.as_deref());
                }
            },
        );
        for ev in model.take_events() {
            if let Some(new_title) = apply_title_event(cwd, explicit_title, &ev) {
                *title = new_title;
                continue;
            }
            match ev {
                ModelEvent::PtyWrite(bytes) => {
                    let _ = pty.write(&bytes);
                }
                ModelEvent::Title(_) | ModelEvent::ResetTitle => unreachable!(),
                ModelEvent::Bell | ModelEvent::Other(_) => {}
            }
        }
    }
}

/// The window side of the graphics pipeline: images become GPU textures and
/// decodes report back through the event loop.
struct WindowGraphicsHost<'a> {
    gpu: Option<&'a mut GpuState>,
    proxy: &'a EventLoopProxy<UserEvent>,
}

impl GraphicsHost for WindowGraphicsHost<'_> {
    fn cell_size(&self) -> (f32, f32) {
        self.gpu
            .as_ref()
            .map_or((8.0, 16.0), |gpu| (gpu.cell_width, gpu.cell_height))
    }

    fn image_stored(&mut self, pane: PaneId, id: u32, image: &DecodedImage) -> bool {
        let Some(gpu) = self.gpu.as_deref_mut() else {
            return false;
        };
        gpu.textures.remove(&(pane, id));
        upload_image_texture(gpu, pane, id, image)
    }

    fn image_displayable(&mut self, pane: PaneId, id: u32, image: &DecodedImage) -> bool {
        let Some(gpu) = self.gpu.as_deref_mut() else {
            return false;
        };
        gpu.textures.contains_key(&(pane, id)) || upload_image_texture(gpu, pane, id, image)
    }

    fn image_freed(&mut self, pane: PaneId, id: u32) {
        if let Some(gpu) = self.gpu.as_deref_mut() {
            gpu.textures.remove(&(pane, id));
        }
    }

    fn start_decode(&mut self, pane: PaneId, anchor: nulix::blocks::GridAnchor, job: DecodeJob) {
        let sender = self.proxy.clone();
        std::thread::spawn(move || {
            let completion = graphics::decode_transfer(job);
            let _ = sender.send_event(UserEvent::GraphicsDecoded(pane, anchor, completion));
        });
    }
}

/// Kitty image data shown in a pane: the server's copy in client mode,
/// otherwise the window's own store.
fn pane_image<'a>(
    remote: Option<&'a RemoteView>,
    store: &'a GraphicsStore,
    pane: PaneId,
    id: u32,
) -> Option<&'a DecodedImage> {
    match remote {
        Some(view) => view.image(pane.0, id),
        None => store.get(pane, id),
    }
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
    image_pipeline: wgpu::RenderPipeline,
    image_bind_group_layout: wgpu::BindGroupLayout,
    image_sampler: wgpu::Sampler,
    textures: BTreeMap<(PaneId, u32), ImageTexture>,
    vertices: Vec<RectVertex>,
    vertex_buffer: wgpu::Buffer,
    vertex_buffer_capacity_bytes: usize,
    image_vertices: Vec<ImageVertex>,
    image_vertex_buffer: wgpu::Buffer,
    image_vertex_buffer_capacity_bytes: usize,
    card_pipeline: wgpu::RenderPipeline,
    card_vertices: Vec<cards::CardVertex>,
    card_vertex_buffer: wgpu::Buffer,
    card_vertex_buffer_capacity_bytes: usize,
}

impl GpuState {
    /// Copies `card_vertices` into the card vertex buffer, growing it first.
    fn upload_card_vertices(&mut self) {
        let required = self.card_vertices.len() * std::mem::size_of::<cards::CardVertex>();
        if required > self.card_vertex_buffer_capacity_bytes {
            self.card_vertex_buffer_capacity_bytes =
                nulix::renderer::next_vertex_buffer_capacity_bytes(
                    self.card_vertex_buffer_capacity_bytes,
                    required,
                );
            self.card_vertex_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("nulix up block card vertex buffer"),
                size: self.card_vertex_buffer_capacity_bytes as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if required > 0 {
            self.queue.write_buffer(
                &self.card_vertex_buffer,
                0,
                bytemuck::cast_slice(&self.card_vertices),
            );
        }
    }

    /// Shapes window-chrome text runs and prepares them for `text_renderer`
    /// over the whole window viewport.
    fn prepare_overlay_texts(
        &mut self,
        texts: &[chrome::OverlayText],
    ) -> Result<(), glyphon::PrepareError> {
        let mut buffers = Vec::with_capacity(texts.len());
        for item in texts {
            let mut buffer = Buffer::new(
                &mut self.font_system,
                Metrics::new(self.font_size, self.cell_height),
            );
            buffer.set_size(Some(item.width.max(1.0)), Some(item.height));
            let attrs = Attrs::new().family(Family::Monospace).color(Color::rgb(
                item.color[0],
                item.color[1],
                item.color[2],
            ));
            buffer.set_text(&item.text, &attrs, cosmic_text::Shaping::Advanced, None);
            buffer.shape_until_scroll(&mut self.font_system, true);
            buffers.push(buffer);
        }
        let areas: Vec<_> = buffers
            .iter()
            .zip(texts)
            .map(|(buffer, item)| TextArea {
                buffer,
                left: item.left,
                top: item.top,
                scale: 1.0,
                bounds: TextBounds {
                    left: item.clip[0],
                    top: item.clip[1],
                    right: item.clip[2],
                    bottom: item.clip[3],
                },
                default_color: Color::rgb(item.color[0], item.color[1], item.color[2]),
                custom_glyphs: &[],
            })
            .collect();
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );
        self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash_cache,
        )
    }
}

/// Draws `nulix up`'s F1 help over the finished frame: the translucent scrim
/// and the rounded panel through the card pipeline, then the panel text.
fn draw_help_overlay(gpu: &mut GpuState, view: &wgpu::TextureView) -> Result<(), String> {
    let (width, height) = (gpu.config.width, gpu.config.height);
    let layout = help::help_layout(
        &help::help_groups(cfg!(target_os = "macos")),
        width,
        height,
        gpu.cell_width,
        gpu.cell_height,
    );
    gpu.card_vertices.clear();
    cards::push_scrim_vertices(
        &mut gpu.card_vertices,
        help::SCRIM,
        help::SCRIM_OPACITY,
        width as f32,
        height as f32,
    );
    cards::push_card_vertices(
        &mut gpu.card_vertices,
        cards::CardRect {
            x0: layout.x0,
            y0: layout.y0,
            x1: layout.x1,
            y1: layout.y1,
        },
        cards::CardStyle {
            fill: chrome::SURFACE,
            border: help::PANEL_BORDER,
            border_px: 1.0,
        },
        width as f32,
        height as f32,
    );
    gpu.upload_card_vertices();
    let panel_visible = layout.x1 > layout.x0 && layout.y1 > layout.y0;
    if panel_visible {
        gpu.prepare_overlay_texts(&layout.texts)
            .map_err(|error| format!("could not prepare the help text: {error}"))?;
    }
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("nulix up help"),
        });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("help scrim, panel and text"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&gpu.card_pipeline);
        pass.set_vertex_buffer(0, gpu.card_vertex_buffer.slice(..));
        pass.draw(0..gpu.card_vertices.len() as u32, 0..1);
        if panel_visible {
            gpu.text_renderer
                .render(&gpu.atlas, &gpu.viewport, &mut pass)
                .map_err(|error| format!("could not render the help text: {error}"))?;
        }
    }
    gpu.queue.submit([encoder.finish()]);
    Ok(())
}

struct ImageTexture {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

#[derive(Clone, Copy)]
struct TextureDraw {
    key: (PaneId, u32),
    start: u32,
}

fn render_texture_draws<'a>(
    pass: &mut wgpu::RenderPass<'a>,
    pipeline: &'a wgpu::RenderPipeline,
    vertex_buffer: &'a wgpu::Buffer,
    textures: &'a BTreeMap<(PaneId, u32), ImageTexture>,
    draws: &'a [TextureDraw],
) {
    if draws.is_empty() {
        return;
    }
    pass.set_pipeline(pipeline);
    pass.set_vertex_buffer(0, vertex_buffer.slice(..));
    for draw in draws {
        if let Some(texture) = textures.get(&draw.key) {
            pass.set_bind_group(0, &texture.bind_group, &[]);
            pass.draw(draw.start..draw.start + 6, 0..1);
        }
    }
}

fn retain_live_textures<T>(
    textures: &mut BTreeMap<(PaneId, u32), T>,
    mut image_exists: impl FnMut(PaneId, u32) -> bool,
) {
    textures.retain(|(pane, id), _| image_exists(*pane, *id));
}

fn upload_image_texture(
    gpu: &mut GpuState,
    pane: PaneId,
    id: u32,
    image: &graphics::DecodedImage,
) -> bool {
    if image.width == 0
        || image.height == 0
        || image.width > gpu.device.limits().max_texture_dimension_2d
        || image.height > gpu.device.limits().max_texture_dimension_2d
    {
        return false;
    }
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("kitty decoded RGBA image"),
        size: wgpu::Extent3d {
            width: image.width,
            height: image.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &image.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(image.width * 4),
            rows_per_image: Some(image.height),
        },
        wgpu::Extent3d {
            width: image.width,
            height: image.height,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("kitty image bind group"),
        layout: &gpu.image_bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&gpu.image_sampler),
            },
        ],
    });
    gpu.textures.insert(
        (pane, id),
        ImageTexture {
            _texture: texture,
            bind_group,
        },
    );
    true
}

fn append_image_quad_vertices(
    vertices: &mut Vec<ImageVertex>,
    quad: nulix::renderer::ImageQuad,
    width: f32,
    height: f32,
) {
    let x0 = quad.destination.x / width * 2.0 - 1.0;
    let x1 = (quad.destination.x + quad.destination.width) / width * 2.0 - 1.0;
    let y0 = 1.0 - quad.destination.y / height * 2.0;
    let y1 = 1.0 - (quad.destination.y + quad.destination.height) / height * 2.0;
    let [u0, v0, u1, v1] = quad.uv;
    for (x, y, u, v) in [
        (x0, y0, u0, v0),
        (x1, y0, u1, v0),
        (x1, y1, u1, v1),
        (x0, y0, u0, v0),
        (x1, y1, u1, v1),
        (x0, y1, u0, v1),
    ] {
        vertices.push(ImageVertex {
            position: [x, y],
            uv: [u, v],
        });
    }
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct RectVertex {
    position: [f32; 2],
    color: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ImageVertex {
    position: [f32; 2],
    uv: [f32; 2],
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TabAction {
    New,
    Close,
    Next,
    Previous,
    Select(usize),
}

/// Classic tab bar height: one cell row, only while several tabs exist.
fn tab_bar_height(tab_count: usize, cell_height: f32) -> u32 {
    chrome::top_bar_height(UiMode::Classic, tab_count, cell_height)
}

/// Classic pane area below the tab bar.
#[cfg(test)]
fn tab_content_rect(window: Rect, tab_count: usize, cell_height: f32) -> Rect {
    chrome::content_rect(UiMode::Classic, window, tab_count, cell_height)
}

fn tab_index_at(x: f64, width: u32, count: usize) -> Option<usize> {
    if !x.is_finite() || x < 0.0 || x >= f64::from(width) || count == 0 {
        return None;
    }
    Some(((x / f64::from(width) * count as f64).floor() as usize).min(count - 1))
}

fn tab_shortcut(
    key: &Key,
    mods: KeyModifiers,
    macos: bool,
    tab_count: usize,
    active_index: usize,
) -> Option<TabAction> {
    let command = if macos { mods.super_key } else { mods.control };
    match key {
        Key::Character(text) if command && mods.shift && text.eq_ignore_ascii_case("t") => {
            Some(TabAction::New)
        }
        Key::Character(text) if command && mods.shift && text.eq_ignore_ascii_case("q") => {
            Some(TabAction::Close)
        }
        Key::Named(NamedKey::Tab) if mods.control && mods.shift => Some(TabAction::Previous),
        Key::Named(NamedKey::Tab) if mods.control => Some(TabAction::Next),
        Key::Character(text) if if macos { mods.super_key } else { mods.alt } => {
            let index = text.chars().next()?.to_digit(10)?.checked_sub(1)? as usize;
            (index < tab_count && index < 9 && index != active_index)
                .then_some(TabAction::Select(index))
        }
        _ => None,
    }
}

fn tab_action_changes_geometry(action: TabAction) -> bool {
    matches!(action, TabAction::New | TabAction::Close)
}

fn tab_style(active: bool) -> ([u8; 3], [u8; 3]) {
    if active {
        ([24, 86, 150], [255, 255, 255])
    } else {
        ([34, 39, 47], [174, 184, 196])
    }
}

/// The classic tab bar: equal-width tabs filling the window width, as drawn
/// before `nulix up` existed.
fn classic_tab_bar_overlay(
    labels: &[String],
    active: usize,
    width: u32,
    bar_height: u32,
) -> (Vec<chrome::OverlayRect>, Vec<chrome::OverlayText>) {
    let count = labels.len();
    let tab_width = width as f32 / count as f32;
    let mut rects = Vec::with_capacity(count);
    let mut texts = Vec::with_capacity(count);
    for (index, label) in labels.iter().enumerate() {
        let left = index as f32 * tab_width;
        let right = if index + 1 == count {
            width as f32
        } else {
            (index + 1) as f32 * tab_width
        };
        let (background, foreground) = tab_style(index == active);
        rects.push(chrome::OverlayRect {
            x0: left,
            y0: 0.0,
            x1: right,
            y1: bar_height as f32,
            color: background,
        });
        texts.push(chrome::OverlayText {
            left: left + 5.0,
            top: 0.0,
            width: right - left - 10.0,
            height: bar_height as f32,
            clip: [0, 0, width as i32, bar_height as i32],
            text: label.clone(),
            color: foreground,
        });
    }
    (rects, texts)
}

fn selection_target(
    origin: Option<PaneId>,
    hit: Option<PaneId>,
    state: PointerState,
) -> Option<PaneId> {
    match state {
        PointerState::Pressed => hit,
        PointerState::Released => origin,
    }
}

fn pane_is_in_active_tab(tabs: &TabSet, pane: PaneId) -> bool {
    tabs.active()
        .is_some_and(|tab| tab.layout_tree().panes().contains(&pane))
}

fn pane_exit_needs_redraw(active_tab_pane: bool, tabs_before: usize, tabs_after: usize) -> bool {
    active_tab_pane || tabs_before != tabs_after
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

fn key_modifiers(state: ModifiersState, caps_lock: bool, num_lock: bool) -> KeyModifiers {
    KeyModifiers {
        shift: state.contains(ModifiersState::SHIFT),
        alt: state.contains(ModifiersState::ALT),
        control: state.contains(ModifiersState::CONTROL),
        super_key: state.contains(ModifiersState::SUPER),
        caps_lock,
        num_lock,
        // winit 0.30 exposes no cross-platform hyper/meta modifier state.
        hyper: false,
        meta: false,
    }
}

fn modifier_event_state(
    mut modifiers: KeyModifiers,
    key: TerminalKey<'_>,
    kind: KeyEventKind,
) -> KeyModifiers {
    let active = kind != KeyEventKind::Release;
    if let TerminalKey::Modifier(modifier) = key {
        match modifier {
            nulix::keyboard::ModifierKey::LeftShift | nulix::keyboard::ModifierKey::RightShift => {
                modifiers.shift = active
            }
            nulix::keyboard::ModifierKey::LeftControl
            | nulix::keyboard::ModifierKey::RightControl => modifiers.control = active,
            nulix::keyboard::ModifierKey::LeftAlt | nulix::keyboard::ModifierKey::RightAlt => {
                modifiers.alt = active
            }
            nulix::keyboard::ModifierKey::LeftSuper | nulix::keyboard::ModifierKey::RightSuper => {
                modifiers.super_key = active
            }
            nulix::keyboard::ModifierKey::CapsLock => {}
            nulix::keyboard::ModifierKey::NumLock => {}
        }
    }
    modifiers
}

fn key_from_winit<'a>(
    key: &'a Key,
    text: Option<&'a str>,
    physical: winit::keyboard::PhysicalKey,
    kitty_active: bool,
) -> Option<KeyInput<'a>> {
    use nulix::keyboard::ModifierKey;
    use winit::keyboard::KeyCode;

    let physical_numpad = match physical {
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad0) => Some(0),
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad1) => Some(1),
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad2) => Some(2),
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad3) => Some(3),
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad4) => Some(4),
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad5) => Some(5),
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad6) => Some(6),
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad7) => Some(7),
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad8) => Some(8),
        winit::keyboard::PhysicalKey::Code(KeyCode::Numpad9) => Some(9),
        _ => None,
    };
    if kitty_active && !matches!(key, Key::Character(_)) {
        let keypad_navigation = match (physical, key) {
            (
                winit::keyboard::PhysicalKey::Code(KeyCode::Numpad0),
                Key::Named(NamedKey::Insert),
            ) => Some(nulix::keyboard::KeypadKey::Insert),
            (winit::keyboard::PhysicalKey::Code(KeyCode::Numpad1), Key::Named(NamedKey::End)) => {
                Some(nulix::keyboard::KeypadKey::End)
            }
            (
                winit::keyboard::PhysicalKey::Code(KeyCode::Numpad2),
                Key::Named(NamedKey::ArrowDown),
            ) => Some(nulix::keyboard::KeypadKey::Down),
            (
                winit::keyboard::PhysicalKey::Code(KeyCode::Numpad3),
                Key::Named(NamedKey::PageDown),
            ) => Some(nulix::keyboard::KeypadKey::PageDown),
            (
                winit::keyboard::PhysicalKey::Code(KeyCode::Numpad4),
                Key::Named(NamedKey::ArrowLeft),
            ) => Some(nulix::keyboard::KeypadKey::Left),
            (winit::keyboard::PhysicalKey::Code(KeyCode::Numpad5), Key::Named(NamedKey::Clear)) => {
                Some(nulix::keyboard::KeypadKey::Begin)
            }
            (
                winit::keyboard::PhysicalKey::Code(KeyCode::Numpad6),
                Key::Named(NamedKey::ArrowRight),
            ) => Some(nulix::keyboard::KeypadKey::Right),
            (winit::keyboard::PhysicalKey::Code(KeyCode::Numpad7), Key::Named(NamedKey::Home)) => {
                Some(nulix::keyboard::KeypadKey::Home)
            }
            (
                winit::keyboard::PhysicalKey::Code(KeyCode::Numpad8),
                Key::Named(NamedKey::ArrowUp),
            ) => Some(nulix::keyboard::KeypadKey::Up),
            (
                winit::keyboard::PhysicalKey::Code(KeyCode::Numpad9),
                Key::Named(NamedKey::PageUp),
            ) => Some(nulix::keyboard::KeypadKey::PageUp),
            (
                winit::keyboard::PhysicalKey::Code(KeyCode::NumpadDecimal),
                Key::Named(NamedKey::Delete),
            ) => Some(nulix::keyboard::KeypadKey::Delete),
            _ => None,
        };
        if let Some(key) = keypad_navigation {
            return Some(KeyInput::new(TerminalKey::NumpadKey(key)));
        }
        if let Some(number) = physical_numpad {
            return Some(KeyInput::new(TerminalKey::Numpad(number)));
        }
        if physical == winit::keyboard::PhysicalKey::Code(KeyCode::NumpadEnter) {
            return Some(KeyInput::new(TerminalKey::NumpadEnter));
        }
        let keypad = match physical {
            winit::keyboard::PhysicalKey::Code(KeyCode::NumpadDecimal) => {
                Some(nulix::keyboard::KeypadKey::Decimal)
            }
            winit::keyboard::PhysicalKey::Code(KeyCode::NumpadDivide) => {
                Some(nulix::keyboard::KeypadKey::Divide)
            }
            winit::keyboard::PhysicalKey::Code(KeyCode::NumpadMultiply) => {
                Some(nulix::keyboard::KeypadKey::Multiply)
            }
            winit::keyboard::PhysicalKey::Code(KeyCode::NumpadSubtract) => {
                Some(nulix::keyboard::KeypadKey::Subtract)
            }
            winit::keyboard::PhysicalKey::Code(KeyCode::NumpadAdd) => {
                Some(nulix::keyboard::KeypadKey::Add)
            }
            winit::keyboard::PhysicalKey::Code(KeyCode::NumpadEqual) => {
                Some(nulix::keyboard::KeypadKey::Equal)
            }
            winit::keyboard::PhysicalKey::Code(KeyCode::NumpadComma) => {
                Some(nulix::keyboard::KeypadKey::Separator)
            }
            _ => None,
        };
        if let Some(keypad) = keypad {
            return Some(KeyInput::new(TerminalKey::NumpadKey(keypad)));
        }
    }

    let terminal_key = match key {
        Key::Named(NamedKey::Enter) => TerminalKey::Enter,
        Key::Named(NamedKey::Tab) => TerminalKey::Tab,
        Key::Named(NamedKey::Backspace) => TerminalKey::Backspace,
        Key::Named(NamedKey::Escape) => TerminalKey::Escape,
        Key::Named(NamedKey::ArrowUp) => TerminalKey::Up,
        Key::Named(NamedKey::ArrowDown) => TerminalKey::Down,
        Key::Named(NamedKey::ArrowLeft) => TerminalKey::Left,
        Key::Named(NamedKey::ArrowRight) => TerminalKey::Right,
        Key::Named(NamedKey::Home) => TerminalKey::Home,
        Key::Named(NamedKey::End) => TerminalKey::End,
        Key::Named(NamedKey::PageUp) => TerminalKey::PageUp,
        Key::Named(NamedKey::PageDown) => TerminalKey::PageDown,
        Key::Named(NamedKey::Insert) => TerminalKey::Insert,
        Key::Named(NamedKey::Delete) => TerminalKey::Delete,
        Key::Named(NamedKey::F1) => TerminalKey::Function(1),
        Key::Named(NamedKey::F2) => TerminalKey::Function(2),
        Key::Named(NamedKey::F3) => TerminalKey::Function(3),
        Key::Named(NamedKey::F4) => TerminalKey::Function(4),
        Key::Named(NamedKey::F5) => TerminalKey::Function(5),
        Key::Named(NamedKey::F6) => TerminalKey::Function(6),
        Key::Named(NamedKey::F7) => TerminalKey::Function(7),
        Key::Named(NamedKey::F8) => TerminalKey::Function(8),
        Key::Named(NamedKey::F9) => TerminalKey::Function(9),
        Key::Named(NamedKey::F10) => TerminalKey::Function(10),
        Key::Named(NamedKey::F11) => TerminalKey::Function(11),
        Key::Named(NamedKey::F12) => TerminalKey::Function(12),
        Key::Named(NamedKey::Shift) => TerminalKey::Modifier(match physical {
            winit::keyboard::PhysicalKey::Code(KeyCode::ShiftRight) => ModifierKey::RightShift,
            _ => ModifierKey::LeftShift,
        }),
        Key::Named(NamedKey::Control) => TerminalKey::Modifier(match physical {
            winit::keyboard::PhysicalKey::Code(KeyCode::ControlRight) => ModifierKey::RightControl,
            _ => ModifierKey::LeftControl,
        }),
        Key::Named(NamedKey::Alt) => TerminalKey::Modifier(match physical {
            winit::keyboard::PhysicalKey::Code(KeyCode::AltRight) => ModifierKey::RightAlt,
            _ => ModifierKey::LeftAlt,
        }),
        Key::Named(NamedKey::Super) => TerminalKey::Modifier(match physical {
            winit::keyboard::PhysicalKey::Code(KeyCode::SuperRight) => ModifierKey::RightSuper,
            _ => ModifierKey::LeftSuper,
        }),
        Key::Named(NamedKey::CapsLock) => TerminalKey::Modifier(ModifierKey::CapsLock),
        Key::Named(NamedKey::NumLock) => TerminalKey::Modifier(ModifierKey::NumLock),
        Key::Named(NamedKey::Space) => TerminalKey::Text(text.unwrap_or(" ")),
        Key::Character(character) => TerminalKey::Text(
            text.filter(|value| !value.is_empty())
                .unwrap_or(character.as_str()),
        ),
        _ => TerminalKey::Text(text.filter(|value| !value.is_empty())?),
    };
    let mut input = KeyInput::new(terminal_key);
    input.associated_text = text;
    if matches!(terminal_key, TerminalKey::Text(_)) {
        input.shifted_key_code = text.and_then(|value| value.chars().next()).map(u32::from);
        let base = physical_ascii_key(physical);
        input.base_layout_key_code = base;
        input.key_code = match terminal_key {
            TerminalKey::Text(value) => value.chars().next().map(|character| {
                if character.is_alphabetic() {
                    character.to_lowercase().next().unwrap_or(character) as u32
                } else {
                    base.unwrap_or(character as u32)
                }
            }),
            _ => None,
        };
        if matches!(key, Key::Named(NamedKey::Space)) {
            input.key_code = Some(32);
        }
    }
    Some(input)
}

fn physical_ascii_key(physical: winit::keyboard::PhysicalKey) -> Option<u32> {
    use winit::keyboard::{KeyCode, PhysicalKey};
    let value = match physical {
        PhysicalKey::Code(KeyCode::KeyA) => 'a',
        PhysicalKey::Code(KeyCode::KeyB) => 'b',
        PhysicalKey::Code(KeyCode::KeyC) => 'c',
        PhysicalKey::Code(KeyCode::KeyD) => 'd',
        PhysicalKey::Code(KeyCode::KeyE) => 'e',
        PhysicalKey::Code(KeyCode::KeyF) => 'f',
        PhysicalKey::Code(KeyCode::KeyG) => 'g',
        PhysicalKey::Code(KeyCode::KeyH) => 'h',
        PhysicalKey::Code(KeyCode::KeyI) => 'i',
        PhysicalKey::Code(KeyCode::KeyJ) => 'j',
        PhysicalKey::Code(KeyCode::KeyK) => 'k',
        PhysicalKey::Code(KeyCode::KeyL) => 'l',
        PhysicalKey::Code(KeyCode::KeyM) => 'm',
        PhysicalKey::Code(KeyCode::KeyN) => 'n',
        PhysicalKey::Code(KeyCode::KeyO) => 'o',
        PhysicalKey::Code(KeyCode::KeyP) => 'p',
        PhysicalKey::Code(KeyCode::KeyQ) => 'q',
        PhysicalKey::Code(KeyCode::KeyR) => 'r',
        PhysicalKey::Code(KeyCode::KeyS) => 's',
        PhysicalKey::Code(KeyCode::KeyT) => 't',
        PhysicalKey::Code(KeyCode::KeyU) => 'u',
        PhysicalKey::Code(KeyCode::KeyV) => 'v',
        PhysicalKey::Code(KeyCode::KeyW) => 'w',
        PhysicalKey::Code(KeyCode::KeyX) => 'x',
        PhysicalKey::Code(KeyCode::KeyY) => 'y',
        PhysicalKey::Code(KeyCode::KeyZ) => 'z',
        PhysicalKey::Code(KeyCode::Digit0) => '0',
        PhysicalKey::Code(KeyCode::Digit1) => '1',
        PhysicalKey::Code(KeyCode::Digit2) => '2',
        PhysicalKey::Code(KeyCode::Digit3) => '3',
        PhysicalKey::Code(KeyCode::Digit4) => '4',
        PhysicalKey::Code(KeyCode::Digit5) => '5',
        PhysicalKey::Code(KeyCode::Digit6) => '6',
        PhysicalKey::Code(KeyCode::Digit7) => '7',
        PhysicalKey::Code(KeyCode::Digit8) => '8',
        PhysicalKey::Code(KeyCode::Digit9) => '9',
        PhysicalKey::Code(KeyCode::Equal) => '=',
        PhysicalKey::Code(KeyCode::Minus) => '-',
        PhysicalKey::Code(KeyCode::BracketLeft) => '[',
        PhysicalKey::Code(KeyCode::BracketRight) => ']',
        PhysicalKey::Code(KeyCode::Backslash) => '\\',
        PhysicalKey::Code(KeyCode::Semicolon) => ';',
        PhysicalKey::Code(KeyCode::Quote) => '\'',
        PhysicalKey::Code(KeyCode::Comma) => ',',
        PhysicalKey::Code(KeyCode::Period) => '.',
        PhysicalKey::Code(KeyCode::Slash) => '/',
        PhysicalKey::Code(KeyCode::Backquote) => '`',
        _ => return None,
    };
    Some(value as u32)
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

fn pane_cell_for_id(
    x: f64,
    y: f64,
    pane: PaneId,
    layouts: &[PaneLayout],
    cell_width: f32,
    cell_height: f32,
) -> Option<(PaneId, usize, usize)> {
    let layout = layouts
        .iter()
        .find(|layout| layout.pane == pane && !layout.hidden)?;
    if !x.is_finite() || !y.is_finite() || cell_width <= 0.0 || cell_height <= 0.0 {
        return None;
    }
    let local_x =
        (x - f64::from(layout.rect.x)).clamp(0.0, f64::from(layout.rect.width.saturating_sub(1)));
    let local_y =
        (y - f64::from(layout.rect.y)).clamp(0.0, f64::from(layout.rect.height.saturating_sub(1)));
    let column = (local_x / f64::from(cell_width)).floor() as usize;
    let row = (local_y / f64::from(cell_height)).floor() as usize;
    Some((
        pane,
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
    let color = color.map(nulix::renderer::srgb_to_linear);
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
    /// `session_socket` selects client mode: no local PTYs, models or control
    /// listener; the window attaches to that session server instead.
    fn new(
        proxy: EventLoopProxy<UserEvent>,
        session_socket: Option<PathBuf>,
        ui_mode: UiMode,
    ) -> Self {
        // Session clients render the server's layout and keep the classic chrome.
        let ui_mode = if session_socket.is_some() {
            UiMode::Classic
        } else {
            ui_mode
        };
        let now = Instant::now();
        let control_server = if session_socket.is_some() {
            None
        } else {
            let control_server = start_control_server(proxy.clone());
            if let Some(server) = &control_server {
                // The value is established before any pane PTY is created and is inherited by every pane shell.
                unsafe { std::env::set_var("NULIX_SOCKET", server.socket_path().as_os_str()) };
            } else {
                unsafe { std::env::remove_var("NULIX_SOCKET") };
            }
            control_server
        };
        Self {
            window: None,
            gpu: None,
            tabs: None,
            panes: BTreeMap::new(),
            selection_origin: None,
            failed: false,
            modifiers: winit::keyboard::ModifiersState::empty(),
            caps_lock: false,
            num_lock: false,
            consumed_keys: ConsumedKeyPresses::default(),
            ime_gate: ImePreeditGate::default(),
            surface_retry: SurfaceRetryBackoff::default(),
            proxy,
            exit_poll: ExitPollBackoff::new(now),
            next_exit_poll: now,
            retry_redraw_at: None,
            running_tick_at: None,
            occluded: false,
            redraw_pending: false,
            cursor_position: PhysicalPosition::new(0.0, 0.0),
            clipboard: None,
            graphics: SharedGraphics::default(),
            _control_server: control_server,
            remote: session_socket.map(RemoteClient::new),
            ui_mode,
            help_open: false,
        }
    }
    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let window = Arc::new(
            event_loop
                .create_window(WindowAttributes::default().with_title("nulix"))
                .map_err(|e| format!("could not create nulix window: {e}"))?,
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
            label: Some("nulix device"),
            ..Default::default()
        }))
        .map_err(|e| format!("could not initialize GPU device: {e}"))?;
        let config = surface
            .get_default_config(&adapter, width, height)
            .ok_or_else(|| "GPU adapter does not support the window surface".to_owned())?;
        surface.configure(&device, &config);
        let mut font_system = FontSystem::new();
        // NULIX_FONT / NULIX_FONT_SIZE override the config file's keys.
        let term_config = nulix::config::current();
        if let Some(font) = std::env::var_os("NULIX_FONT")
            .map(PathBuf::from)
            .or_else(|| term_config.font.clone())
        {
            font_system.db_mut().load_font_file(font).ok();
        }
        let font_size = std::env::var("NULIX_FONT_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|value: &f32| {
                value.is_finite() && nulix::config::FONT_SIZE_RANGE.contains(value)
            })
            .or(term_config.font_size)
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
        let image_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("kitty image texture layout"),
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
        let image_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kitty image sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let image_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kitty image shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
        });
        let image_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("kitty image pipeline layout"),
                bind_group_layouts: &[Some(&image_bind_group_layout)],
                immediate_size: 0,
            });
        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("kitty image pipeline"),
            layout: Some(&image_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &image_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ImageVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &image_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let card_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nulix up block card shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("card.wgsl").into()),
        });
        let card_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nulix up block card pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &card_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<cards::CardVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x2,
                        3 => Float32x2,
                        4 => Float32x3,
                        5 => Float32x3
                    ],
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &card_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let card_vertex_buffer_capacity_bytes = 6 * std::mem::size_of::<cards::CardVertex>();
        let card_vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nulix up block card vertex buffer"),
            size: card_vertex_buffer_capacity_bytes as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let vertex_buffer_capacity_bytes = 1024;
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terminal reusable cell vertex buffer"),
            size: vertex_buffer_capacity_bytes as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let image_vertex_buffer_capacity_bytes = 6 * std::mem::size_of::<ImageVertex>();
        let image_vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kitty image quad vertex buffer"),
            size: image_vertex_buffer_capacity_bytes as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.window = Some(Arc::clone(&window));
        if let Some(remote) = self.remote.as_mut() {
            // The attach itself waits in about_to_wait until the size settled.
            remote.settle = AttachSettle::new(Instant::now());
        } else {
            self.create_initial_tab(width, height, physical_cell_width, physical_cell_height)?;
        }
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
            image_pipeline,
            image_bind_group_layout,
            image_sampler,
            textures: BTreeMap::new(),
            vertices: Vec::new(),
            vertex_buffer,
            vertex_buffer_capacity_bytes,
            image_vertices: Vec::new(),
            image_vertex_buffer,
            image_vertex_buffer_capacity_bytes,
            card_pipeline,
            card_vertices: Vec::new(),
            card_vertex_buffer,
            card_vertex_buffer_capacity_bytes,
        });
        Ok(())
    }

    fn create_initial_tab(
        &mut self,
        width: u32,
        height: u32,
        physical_cell_width: f32,
        physical_cell_height: f32,
    ) -> Result<(), String> {
        let tabs = TabSet::new(
            chrome::content_rect(
                self.ui_mode,
                Rect::new(0, 0, width, height),
                1,
                physical_cell_height,
            ),
            CellSize::new(
                physical_cell_width.ceil() as u32,
                physical_cell_height.ceil() as u32,
            ),
            4,
            20,
            5,
        );
        let pane_id = tabs.active().expect("initial tab exists").active_pane();
        let (cols, rows) = tabs
            .active()
            .expect("initial tab exists")
            .layout()
            .into_iter()
            .find(|layout| layout.pane == pane_id && !layout.hidden)
            .map(|layout| {
                chrome::pane_grid_layout(self.ui_mode, layout, physical_cell_width.ceil() as u32)
            })
            .map(|layout| (layout.cols as usize, layout.rows as usize))
            .unwrap_or_else(|| grid_size(width, height, physical_cell_width, physical_cell_height));
        self.tabs = Some(tabs);
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
        let pty = nulix::pty::Pty::spawn_shell_notified(
            cwd.clone(),
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
            model: Model::with_scrollback_limit(cols, rows, nulix::config::current().scrollback),
            graphics: PaneGraphics::default(),
            title: cwd_basename_title(&cwd),
            explicit_title: None,
            cwd,
            wakeup,
            wheel: WheelAccumulator::default(),
            selection_router: SelectionRouter::default(),
            glyphs: GlyphCache::default(),
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        let (w, h) = clamp_surface_size(width, height);
        gpu.config.width = w;
        gpu.config.height = h;
        gpu.surface.configure(&gpu.device, &gpu.config);
        if let Some(remote) = self.remote.as_mut() {
            if remote.connection.is_none() {
                // Not attached yet: the settled size goes into `Hello` instead.
                remote.settle.resized(Instant::now());
                return;
            }
            // The server adopts the window metrics and answers with layout and full frames.
            for state in remote.panes.values_mut() {
                state.glyphs.invalidate();
            }
            remote.send(WindowMetrics::new(w, h, gpu.cell_width, gpu.cell_height).resize());
            return;
        }
        let Some(tabs) = self.tabs.as_mut() else {
            return;
        };
        let cell = CellSize::new(gpu.cell_width.ceil() as u32, gpu.cell_height.ceil() as u32);
        let layouts = chrome::relayout_tabs(
            self.ui_mode,
            tabs,
            Rect::new(0, 0, w, h),
            cell,
            gpu.cell_height,
        );
        for layout in layouts {
            if let Some(pane) = self.panes.get_mut(&layout.pane) {
                if !layout.hidden {
                    pane.graphics.resize_model(
                        &mut pane.model,
                        layout.cols as usize,
                        layout.rows as usize,
                    );
                    let _ = pane.pty.resize(
                        layout.cols.min(u16::MAX as u32) as u16,
                        layout.rows.min(u16::MAX as u32) as u16,
                        layout.rect.width.min(u16::MAX as u32) as u16,
                        layout.rect.height.min(u16::MAX as u32) as u16,
                    );
                }
                pane.glyphs.invalidate();
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
        if let Some(remote) = &self.remote {
            return remote.view.focused_pane();
        }
        self.tabs.as_ref()?.active().map(Tab::active_pane)
    }

    fn tab_count(&self) -> usize {
        if let Some(remote) = &self.remote {
            return remote.view.tab_count();
        }
        self.tabs.as_ref().map_or(0, |tabs| tabs.tabs().len())
    }

    fn active_tab_index(&self) -> usize {
        if let Some(remote) = &self.remote {
            return remote.view.active_tab_index();
        }
        self.tabs
            .as_ref()
            .and_then(TabSet::active_index)
            .unwrap_or(0)
    }

    fn focused_modes(&self) -> Option<FocusedModes> {
        let focused = self.focused_pane_id()?;
        if let Some(remote) = &self.remote {
            let pane = remote.view.pane(focused.0)?;
            return Some(FocusedModes {
                keyboard_flags: pane.keyboard_flags(),
                app_cursor: pane.modes.app_cursor,
                bracketed_paste: pane.modes.bracketed_paste,
                alternate_screen: pane.modes.alternate_screen,
            });
        }
        let model = &self.panes.get(&focused)?.model;
        Some(FocusedModes {
            keyboard_flags: model.keyboard_flags(),
            app_cursor: model.application_cursor_keys(),
            bracketed_paste: model.bracketed_paste_enabled(),
            alternate_screen: model.alternate_screen_active(),
        })
    }

    /// Writes already-encoded input to the focused pane: its PTY locally, or an
    /// `Input` message that the server writes to the pane PTY.
    fn write_focused(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let Some(focused) = self.focused_pane_id() else {
            return Ok(());
        };
        if let Some(remote) = &self.remote {
            remote.send(ClientMessage::Input {
                pane: focused.0,
                data: bytes.to_vec(),
            });
            return Ok(());
        }
        match self.panes.get_mut(&focused) {
            Some(pane) => pane.pty.write(bytes),
            None => Ok(()),
        }
    }

    /// Local panes follow the live bottom after input; the server does the same
    /// for every `Input` message, so client mode needs no extra message.
    fn scroll_focused_to_bottom(&mut self) {
        if let Some(pane) = self.focused_pane_mut() {
            pane.model.scroll_to_bottom();
        }
    }

    fn selection_router_mut(&mut self, pane: PaneId) -> Option<&mut SelectionRouter> {
        if let Some(remote) = self.remote.as_mut() {
            return Some(&mut remote.pane_state(pane).selection_router);
        }
        self.panes
            .get_mut(&pane)
            .map(|pane| &mut pane.selection_router)
    }

    fn selection_op(&mut self, pane: PaneId, op: SelectionOp) {
        if let Some(remote) = &self.remote {
            remote.send(ClientMessage::Selection { pane: pane.0, op });
            return;
        }
        let Some(pane) = self.panes.get_mut(&pane) else {
            return;
        };
        match op {
            SelectionOp::Begin { column, row, kind } => {
                pane.model.begin_selection(column, row, kind)
            }
            SelectionOp::Extend { column, row } => pane.model.extend_selection(column, row),
            SelectionOp::Clear => pane.model.clear_selection(),
        }
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
            let tab_count_before = self.tabs.as_ref().map_or(0, |tabs| tabs.tabs().len());
            let active_pane_exited = self
                .tabs
                .as_ref()
                .is_some_and(|tabs| pane_is_in_active_tab(tabs, pane));
            let close_window = self
                .tabs
                .as_mut()
                .is_none_or(|tabs| close_pane_lifecycle(tabs, pane));
            self.panes.remove(&pane);
            self.graphics.store.remove_pane(pane);
            if close_window {
                event_loop.exit();
                return;
            }
            if let Some(gpu) = self.gpu.as_ref() {
                self.resize(gpu.config.width, gpu.config.height);
            }
            self.refresh_window_title();
            let tab_count_after = self.tabs.as_ref().map_or(0, |tabs| tabs.tabs().len());
            if pane_exit_needs_redraw(active_pane_exited, tab_count_before, tab_count_after) {
                self.request_redraw();
            }
        }
        if !self.panes.is_empty()
            && let ExitPollDecision::RetryAt(deadline) =
                self.exit_poll.observe_wait_status(false, Instant::now())
        {
            self.next_exit_poll = deadline;
        }
    }

    fn pane_layouts(&self) -> Vec<PaneLayout> {
        if let Some(remote) = &self.remote {
            return remote.view.pane_layouts();
        }
        let layouts = self
            .tabs
            .as_ref()
            .and_then(TabSet::active)
            .map_or_else(Vec::new, Tab::layout);
        let Some(cell_width) = self.layout_cell_width() else {
            return layouts;
        };
        layouts
            .into_iter()
            .map(|layout| chrome::pane_grid_layout(self.ui_mode, layout, cell_width))
            .collect()
    }

    /// Whole-pixel cell width the pane layout uses.
    fn layout_cell_width(&self) -> Option<u32> {
        self.gpu.as_ref().map(|gpu| gpu.cell_width.ceil() as u32)
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
        if let Some(remote) = self.remote.as_mut() {
            let direction = match direction {
                Direction::Horizontal => SplitDirection::Right,
                Direction::Vertical => SplitDirection::Down,
            };
            remote.layout(LayoutAction::Split { direction });
            return Ok(());
        }
        let pane_id = self
            .tabs
            .as_mut()
            .and_then(TabSet::active_mut)
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
                if let Some(tab) = self.tabs.as_mut().and_then(TabSet::active_mut) {
                    let _ = tab.close_pane(pane_id);
                }
                Err(error)
            }
        }
    }

    fn close_focused(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(remote) = self.remote.as_mut() {
            remote.layout(LayoutAction::ClosePane);
            return;
        }
        let Some(pane_id) = self.focused_pane_id() else {
            return;
        };
        let close_window = self
            .tabs
            .as_mut()
            .is_none_or(|tabs| close_pane_lifecycle(tabs, pane_id));
        if close_window {
            self.panes.remove(&pane_id);
            self.graphics.store.remove_pane(pane_id);
            event_loop.exit();
            return;
        }
        self.panes.remove(&pane_id);
        self.graphics.store.remove_pane(pane_id);
        if let Some(gpu) = self.gpu.as_ref() {
            self.resize(gpu.config.width, gpu.config.height);
        }
        self.refresh_window_title();
        self.request_redraw();
    }

    fn focus_direction(&mut self, direction: FocusDirection) {
        if let Some(remote) = self.remote.as_mut() {
            let direction = match direction {
                FocusDirection::Left => RemoteFocusDirection::Left,
                FocusDirection::Right => RemoteFocusDirection::Right,
                FocusDirection::Up => RemoteFocusDirection::Up,
                FocusDirection::Down => RemoteFocusDirection::Down,
            };
            remote.layout(LayoutAction::FocusDirection { direction });
            return;
        }
        if let Some(tab) = self.tabs.as_mut().and_then(TabSet::active_mut) {
            tab.move_focus(direction);
        }
        self.request_redraw();
    }

    /// Labels of the standalone window's tabs: each active pane's title.
    fn local_tab_labels(&self) -> Vec<String> {
        self.tabs.as_ref().map_or_else(Vec::new, |tabs| {
            tabs.tabs()
                .iter()
                .map(|tab| {
                    self.panes
                        .get(&tab.active_pane())
                        .map(|pane| pane.title.clone())
                        .unwrap_or_else(|| "shellux".into())
                })
                .collect()
        })
    }

    fn refresh_window_title(&self) {
        let Some(window) = &self.window else { return };
        if let Some(remote) = &self.remote {
            let title = remote.view.active_title().unwrap_or("shellux");
            window.set_title(&format!("nulix — {title}"));
            return;
        }
        let title = self
            .tabs
            .as_ref()
            .and_then(TabSet::active)
            .and_then(|tab| self.panes.get(&tab.active_pane()))
            .map(|pane| pane.title.as_str())
            .unwrap_or("shellux");
        window.set_title(&format!("nulix — {title}"));
    }

    fn new_tab(&mut self) -> Result<(), String> {
        let cwd = self
            .focused_pane()
            .map(|pane| pane.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let proxy = self.proxy.clone();
        let ui_mode = self.ui_mode;
        let cell_width = self.layout_cell_width().unwrap_or(1);
        let result = {
            let tabs = self.tabs.as_mut().ok_or("tab set is not initialized")?;
            create_tab_with(tabs, |pane_id, layout| {
                let layout = chrome::pane_grid_layout(ui_mode, layout, cell_width);
                Self::create_pane(
                    pane_id,
                    layout.cols as usize,
                    layout.rows as usize,
                    cwd,
                    proxy,
                )
            })
        };
        let (pane_id, pane) = result?;
        self.panes.insert(pane_id, pane);
        self.refresh_window_title();
        self.request_redraw();
        Ok(())
    }

    fn control_command(
        &mut self,
        request: ControlRequest,
        event_loop: &ActiveEventLoop,
    ) -> ControlResponse {
        let id = Some(request.id);
        let fail = |code, message: String| ControlResponse {
            id,
            outcome: ControlOutcome::Failure(ControlError::for_request(id, code, message)),
        };
        let result = match request.command {
            ControlCommand::List => {
                let Some(tabs) = self.tabs.as_ref() else {
                    return fail(
                        ControlErrorCode::Unavailable,
                        "pane layout is not initialized".into(),
                    );
                };
                let active = tabs.active_index().unwrap_or(0);
                let values: Vec<_> = tabs.tabs().iter().enumerate().map(|(index, tab)| {
                    let panes: Vec<_> = tab.layout_tree().panes().iter().map(|pane_id| {
                        let pane = self.panes.get(pane_id).map(|pane| control::ListPane {
                            cwd: &pane.cwd,
                            title: &pane.title,
                            model: &pane.model,
                        });
                        let focused = index == active && tab.active_pane() == *pane_id;
                        control::list_pane_entry(pane_id.0, focused, pane)
                    }).collect();
                    serde_json::json!({"index": index, "active": index == active, "panes": panes})
                }).collect();
                serde_json::json!({"tabs": values})
            }
            ControlCommand::NewTab => match self.new_tab() {
                Ok(()) => {
                    let index = self
                        .tabs
                        .as_ref()
                        .and_then(|tabs| tabs.active_index())
                        .unwrap_or(0);
                    let pane = self
                        .tabs
                        .as_ref()
                        .and_then(|tabs| tabs.active())
                        .map(|tab| tab.active_pane().0)
                        .unwrap_or(0);
                    serde_json::json!({"index": index, "pane": pane})
                }
                Err(error) => return fail(ControlErrorCode::OperationRefused, error),
            },
            ControlCommand::Split { pane, direction } => {
                if let Some(pane_id) = pane
                    && !self.focus_pane_id(PaneId(pane_id))
                {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {pane_id} does not exist"),
                    );
                }
                let direction = match direction {
                    control::ControlDirection::Right => Direction::Horizontal,
                    control::ControlDirection::Down => Direction::Vertical,
                };
                match self.split_focused(direction) {
                    Ok(()) => {
                        serde_json::json!({"pane": self.focused_pane_id().map(|pane| pane.0)})
                    }
                    Err(error) => return fail(ControlErrorCode::OperationRefused, error),
                }
            }
            ControlCommand::Focus { pane } => {
                if !self.focus_pane_id(PaneId(pane)) {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {pane} does not exist"),
                    );
                }
                self.request_redraw();
                serde_json::json!({"pane": pane})
            }
            ControlCommand::Close { pane } => {
                let pane_id = PaneId(pane);
                let Some(tabs) = self.tabs.as_mut() else {
                    return fail(
                        ControlErrorCode::Unavailable,
                        "pane layout is not initialized".into(),
                    );
                };
                if !tabs
                    .tabs()
                    .iter()
                    .any(|tab| tab.layout_tree().panes().contains(&pane_id))
                {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {pane} does not exist"),
                    );
                }
                let close_window = close_pane_lifecycle(tabs, pane_id);
                self.panes.remove(&pane_id);
                self.graphics.store.remove_pane(pane_id);
                if close_window {
                    self.panes.clear();
                    self.graphics.store = GraphicsStore::default();
                    if let Some(gpu) = self.gpu.as_mut() {
                        gpu.textures.clear();
                    }
                    event_loop.exit();
                } else {
                    if let Some(gpu) = self.gpu.as_ref() {
                        self.resize(gpu.config.width, gpu.config.height);
                    }
                    self.refresh_window_title();
                    self.request_redraw();
                }
                serde_json::json!({"closed": pane})
            }
            ControlCommand::SendText { pane, text } => {
                let pane_id = pane.map(PaneId).or_else(|| self.focused_pane_id());
                let Some(pane_id) = pane_id else {
                    return fail(ControlErrorCode::Unavailable, "no focused pane".into());
                };
                let Some(target) = self.panes.get_mut(&pane_id) else {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {} does not exist", pane_id.0),
                    );
                };
                if let Err(error) = target.pty.write(text.as_bytes()) {
                    return fail(
                        ControlErrorCode::Unavailable,
                        format!("could not write to PTY: {error}"),
                    );
                }
                target.model.scroll_to_bottom();
                self.request_redraw();
                serde_json::json!({"pane": pane_id.0, "written": text.len()})
            }
            ControlCommand::Block { pane, block } => {
                let Some(pane_id) = pane.map(PaneId).or_else(|| self.focused_pane_id()) else {
                    return fail(ControlErrorCode::Unavailable, "no focused pane".into());
                };
                let Some(target) = self.panes.get(&pane_id) else {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {} does not exist", pane_id.0),
                    );
                };
                match control::block_result(pane_id.0, &target.model, block) {
                    Ok(result) => result,
                    Err((code, message)) => return fail(code, message),
                }
            }
            ControlCommand::GetText { pane, scope } => {
                let pane_id = pane.map(PaneId).or_else(|| self.focused_pane_id());
                let Some(pane_id) = pane_id else {
                    return fail(ControlErrorCode::Unavailable, "no focused pane".into());
                };
                let Some(target) = self.panes.get(&pane_id) else {
                    return fail(
                        ControlErrorCode::NotFound,
                        format!("pane {} does not exist", pane_id.0),
                    );
                };
                let text = match scope {
                    TextScope::Screen => Some(target.model.visible_text()),
                    TextScope::LastBlock => target.model.last_finished_block_output_text(),
                };
                let Some(text) = text else {
                    return fail(
                        ControlErrorCode::OperationRefused,
                        "last finished block is unavailable or its anchors were evicted".into(),
                    );
                };
                serde_json::json!({"pane": pane_id.0, "text": text})
            }
            ControlCommand::Shutdown => {
                return fail(
                    ControlErrorCode::OperationRefused,
                    "shutdown is only supported by nulix-server".into(),
                );
            }
            ControlCommand::Attach => {
                return fail(
                    ControlErrorCode::OperationRefused,
                    "attach is only supported by nulix-server".into(),
                );
            }
            ControlCommand::Ui { mode } => {
                self.switch_ui_mode(mode.map(chrome::ui_mode_from_control));
                control::ui_result(chrome::control_ui_mode(self.ui_mode))
            }
        };
        ControlResponse {
            id,
            outcome: ControlOutcome::Success(result),
        }
    }

    /// `nulix up` / `nulix down` inside this window (control `ui`): switches
    /// the chrome at runtime. Every tab is laid out again through the window
    /// resize path, because bar rows and the card inset differ between modes,
    /// so PTY grids follow; pane ids and tab order are untouched.
    fn switch_ui_mode(&mut self, requested: Option<UiMode>) {
        let switch = chrome::switch_ui_mode(self.ui_mode, requested, self.help_open);
        self.help_open = switch.help_open;
        if !switch.changed {
            return;
        }
        self.ui_mode = switch.mode;
        // The running-card tick is re-armed by the next redraw in up mode.
        self.running_tick_at = None;
        if let Some(gpu) = self.gpu.as_ref() {
            self.resize(gpu.config.width, gpu.config.height);
        }
        self.request_redraw();
    }

    fn focus_pane_id(&mut self, pane: PaneId) -> bool {
        let Some(tabs) = self.tabs.as_mut() else {
            return false;
        };
        let Some(index) = tabs.tab_index_for_pane(pane) else {
            return false;
        };
        tabs.select(index);
        tabs.tab_mut(index).is_some_and(|tab| tab.focus_pane(pane))
    }

    fn close_active_tab(&mut self, event_loop: &ActiveEventLoop) {
        let Some(tabs) = self.tabs.as_mut() else {
            return;
        };
        let Some(active) = tabs.active() else { return };
        let panes = active.layout_tree().panes();
        if matches!(tabs.close_active(), nulix::layout::TabCloseResult::LastTab) {
            self.panes.clear();
            self.graphics.store = GraphicsStore::default();
            if let Some(gpu) = self.gpu.as_mut() {
                gpu.textures.clear();
            }
            event_loop.exit();
            return;
        }
        for pane in panes {
            self.panes.remove(&pane);
            self.graphics.store.remove_pane(pane);
            if let Some(gpu) = self.gpu.as_mut() {
                gpu.textures
                    .retain(|(cached_pane, _), _| *cached_pane != pane);
            }
        }
        self.refresh_window_title();
        self.request_redraw();
    }

    /// Handles a primary press on `nulix up`'s bars: select, close (`×`) or
    /// create (`+`) tabs through the tab actions; the hint bar ignores clicks.
    /// Returns whether the press landed on a bar.
    fn up_chrome_click(&mut self, event_loop: &ActiveEventLoop) -> bool {
        if self.ui_mode != UiMode::Up || self.remote.is_some() {
            return false;
        }
        let Some(gpu) = self.gpu.as_ref() else {
            return false;
        };
        let (x, y) = (self.cursor_position.x, self.cursor_position.y);
        let window_height = f64::from(gpu.config.height);
        let top = chrome::top_bar_height(UiMode::Up, self.tab_count(), gpu.cell_height);
        let bottom = chrome::bottom_bar_height(UiMode::Up, gpu.cell_height);
        if y >= window_height - f64::from(bottom) && y < window_height {
            return true;
        }
        if !(0.0..f64::from(top)).contains(&y) {
            return false;
        }
        let Some(column) = chrome::column_at(x, gpu.cell_width, gpu.config.width) else {
            return true;
        };
        let columns = (gpu.config.width as f32 / gpu.cell_width).floor() as u32;
        let labels = self.local_tab_labels();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let bar = chrome::up_tab_bar(&labels, columns);
        match chrome::up_tab_bar_hit(&bar, column) {
            Some(chrome::TabBarHit::Select(index)) => {
                self.apply_tab_action(TabAction::Select(index), event_loop);
            }
            Some(chrome::TabBarHit::Close(index)) => {
                let active = self.active_tab_index();
                self.apply_tab_action(TabAction::Select(index), event_loop);
                self.apply_tab_action(TabAction::Close, event_loop);
                if let Some(previous) = chrome::active_after_closing(index, active) {
                    self.apply_tab_action(TabAction::Select(previous), event_loop);
                }
            }
            Some(chrome::TabBarHit::New) => self.apply_tab_action(TabAction::New, event_loop),
            None => {}
        }
        true
    }

    fn apply_tab_action(&mut self, action: TabAction, event_loop: &ActiveEventLoop) {
        if let Some(remote) = self.remote.as_mut() {
            remote.layout(match action {
                TabAction::New => LayoutAction::NewTab,
                TabAction::Close => LayoutAction::CloseTab,
                TabAction::Next => LayoutAction::NextTab,
                TabAction::Previous => LayoutAction::PreviousTab,
                TabAction::Select(index) => LayoutAction::SelectTab { index },
            });
            return;
        }
        let geometry_changed = tab_action_changes_geometry(action);
        match action {
            TabAction::New => {
                if let Err(error) = self.new_tab() {
                    eprintln!("nulix: {error}");
                }
            }
            TabAction::Close => self.close_active_tab(event_loop),
            TabAction::Next => {
                if let Some(tabs) = self.tabs.as_mut() {
                    tabs.next();
                }
            }
            TabAction::Previous => {
                if let Some(tabs) = self.tabs.as_mut() {
                    tabs.prev();
                }
            }
            TabAction::Select(index) => {
                if let Some(tabs) = self.tabs.as_mut() {
                    tabs.select(index);
                }
            }
        }
        if geometry_changed && let Some(gpu) = self.gpu.as_ref() {
            self.resize(gpu.config.width, gpu.config.height);
        }
        self.refresh_window_title();
        self.request_redraw();
    }

    fn clipboard(&mut self) -> Option<&mut Clipboard> {
        if self.clipboard.is_none() {
            match Clipboard::new() {
                Ok(clipboard) => self.clipboard = Some(clipboard),
                Err(error) => {
                    eprintln!("nulix: clipboard initialization failed: {error}");
                    return None;
                }
            }
        }
        self.clipboard.as_mut()
    }

    fn copy_selection(&mut self) {
        if let (Some(focused), Some(remote)) = (self.focused_pane_id(), self.remote.as_mut()) {
            // The clipboard is set when the matching `CopyText` arrives.
            remote.copy(focused, CopySource::Selection);
            return;
        }
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
            eprintln!("nulix: clipboard copy failed: {error}");
        }
    }

    fn copy_block_text(&mut self, input: bool) {
        if let (Some(focused), Some(remote)) = (self.focused_pane_id(), self.remote.as_mut()) {
            let source = if input {
                CopySource::BlockInput
            } else {
                CopySource::BlockOutput
            };
            remote.copy(focused, source);
            return;
        }
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
            eprintln!("nulix: clipboard copy failed: {error}");
        }
    }

    fn handle_block_shortcut(&mut self, action: BlockShortcut) {
        if let (Some(focused), Some(remote)) = (self.focused_pane_id(), self.remote.as_ref()) {
            let op = match action {
                BlockShortcut::Previous => Some(BlockOp::Previous),
                BlockShortcut::Next => Some(BlockOp::Next),
                BlockShortcut::Select => Some(BlockOp::Select),
                BlockShortcut::CopyOutput | BlockShortcut::CopyInput => None,
            };
            if let Some(op) = op {
                remote.send(ClientMessage::Block {
                    pane: focused.0,
                    op,
                });
                return;
            }
        }
        match action {
            BlockShortcut::Previous => {
                if let Some(pane) = self.focused_pane_mut() {
                    pane.model
                        .navigate_block(nulix::block_navigation::NavigationDirection::Previous);
                }
            }
            BlockShortcut::Next => {
                if let Some(pane) = self.focused_pane_mut() {
                    pane.model
                        .navigate_block(nulix::block_navigation::NavigationDirection::Next);
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
                    eprintln!("nulix: clipboard paste failed: {error}");
                    None
                }
            }) {
            Some(text) => text,
            None => return,
        };
        let bracketed = self
            .focused_modes()
            .is_some_and(|modes| modes.bracketed_paste);
        let bytes = encode_paste(&text, bracketed);
        if let Err(error) = self.write_focused(&bytes) {
            eprintln!("nulix: clipboard paste write failed: {error}");
        }
        self.scroll_focused_to_bottom();
        self.request_redraw();
    }

    /// Applies one attach-stream event in client mode.
    fn remote_event(&mut self, event: RemoteEvent, event_loop: &ActiveEventLoop) {
        let Some(remote) = self.remote.as_mut() else {
            return;
        };
        match event {
            RemoteEvent::Message(ServerMessage::CopyText { request, text }) => {
                if remote.copy_requests.remove(&request)
                    && let Some(text) = text
                    && let Some(clipboard) = self.clipboard()
                    && let Err(error) = clipboard.set_text(text)
                {
                    eprintln!("nulix: clipboard copy failed: {error}");
                }
            }
            RemoteEvent::Message(ServerMessage::LayoutResult { error, .. }) => {
                if let Some(error) = error {
                    eprintln!("nulix: {error}");
                }
            }
            RemoteEvent::Message(ServerMessage::Error { code, message }) => {
                eprintln!("nulix: session server error ({code}): {message}");
            }
            RemoteEvent::Message(ServerMessage::Exit { .. }) => {
                remote.closing = true;
                event_loop.exit();
            }
            RemoteEvent::Message(message) => match remote.view.apply(message) {
                ViewChange::Layout { removed } => {
                    for pane in removed {
                        remote.panes.remove(&PaneId(pane));
                        if self.selection_origin == Some(PaneId(pane)) {
                            self.selection_origin = None;
                        }
                    }
                    self.refresh_window_title();
                    self.request_redraw();
                }
                ViewChange::Frame { pane, outcome } => match outcome {
                    FrameOutcome::Applied => self.request_redraw(),
                    FrameOutcome::ResyncNeeded => {
                        remote.send(ClientMessage::Resync { pane: Some(pane) })
                    }
                    FrameOutcome::Ignored => {}
                },
                ViewChange::Image {
                    pane,
                    image,
                    change,
                } => {
                    let key = (PaneId(pane), image);
                    if let Some(gpu) = self.gpu.as_mut() {
                        // A replaced or removed image never keeps its old texture.
                        gpu.textures.remove(&key);
                        if change == ImageChange::Ready
                            && let Some(decoded) = remote.view.image(pane, image)
                        {
                            upload_image_texture(gpu, key.0, key.1, decoded);
                        }
                    }
                    if let ImageChange::Rejected(reason) = change {
                        eprintln!("nulix: dropped kitty image from session server: {reason}");
                    }
                    self.request_redraw();
                }
                ViewChange::None => {}
            },
            RemoteEvent::Closed(diagnostic) => {
                if !remote.closing {
                    remote.closing = true;
                    eprintln!(
                        "nulix: {}",
                        diagnostic
                            .as_deref()
                            .unwrap_or("session server closed the attach stream")
                    );
                    self.failed = true;
                }
                event_loop.exit();
            }
        }
    }

    /// Wheel in client mode: scrollback on the server, or arrow keys for a
    /// full-screen application on the alternate screen (alternate scroll).
    /// Mouse-reporting applications on the primary screen get nothing, as in
    /// the standalone window.
    fn remote_wheel(&mut self, delta: MouseScrollDelta) {
        let Some((pane_id, _, _)) = self.pane_and_cell_at_cursor() else {
            return;
        };
        let Some(remote) = self.remote.as_mut() else {
            return;
        };
        let Some(pane) = remote.view.pane(pane_id.0) else {
            return;
        };
        let (modes, flags) = (pane.modes, pane.keyboard_flags());
        let line_delta = match delta {
            MouseScrollDelta::LineDelta(_, y) => f64::from(y),
            MouseScrollDelta::PixelDelta(pos) => pos.y / 16.0,
        };
        let lines = remote.pane_state(pane_id).wheel.push_lines(line_delta);
        if lines == 0 {
            return;
        }
        if !modes.mouse_or_alternate {
            remote.send(ClientMessage::Scroll {
                pane: pane_id.0,
                lines,
            });
        } else if modes.alternate_screen {
            let key = if lines > 0 {
                TerminalKey::Up
            } else {
                TerminalKey::Down
            };
            let Some(bytes) = encode_key_with_flags(
                KeyInput::new(key),
                KeyModifiers::default(),
                modes.app_cursor,
                flags,
                KeyEventKind::Press,
            ) else {
                return;
            };
            remote.send(ClientMessage::Input {
                pane: pane_id.0,
                data: bytes.repeat(lines.unsigned_abs() as usize),
            });
        }
    }

    fn draw(&mut self) -> Result<FrameRecovery, String> {
        let layouts = self.pane_layouts();
        let focused = self.focused_pane_id();
        let tabs = self.tabs.as_ref();
        let tab_count = self.tab_count();
        let active_tab_index = self.active_tab_index();
        let tab_bar_height = self
            .gpu
            .as_ref()
            .map_or(0, |gpu| tab_bar_height(tab_count, gpu.cell_height));
        let (dividers, tab_labels): (Vec<Rect>, Vec<String>) = if let Some(remote) = &self.remote {
            (remote.view.divider_rects(), remote.view.tab_titles())
        } else {
            let dividers = tabs.and_then(TabSet::active).map_or_else(Vec::new, |tab| {
                tab.layout_tree().divider_rects(tab.active_pane())
            });
            (dividers, self.local_tab_labels())
        };
        let focused_activity = focused
            .and_then(|pane| self.panes.get(&pane))
            .map_or(PaneActivity::Unknown, |pane| pane.model.blocks().activity());
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
                return Ok(nulix::renderer::frame_recovery(SurfaceFailure::Timeout));
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(nulix::renderer::frame_recovery(SurfaceFailure::Occluded));
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
        let graphics_store = &self.graphics.store;
        let (remote_view, mut remote_panes) = match self.remote.as_mut() {
            Some(remote) => (Some(&remote.view), Some(&mut remote.panes)),
            None => (None, None),
        };
        retain_live_textures(&mut gpu.textures, |pane, id| {
            pane_image(remote_view, graphics_store, pane, id).is_some()
        });
        let panes = &mut self.panes;
        // `nulix up` draws command blocks as rounded cards (standalone window only).
        let up_cards = self.ui_mode == UiMode::Up && remote_view.is_none();
        let layout_cell_width = gpu.cell_width.ceil() as u32;
        let now = std::time::SystemTime::now();
        for layout in layouts.iter().filter(|layout| !layout.hidden) {
            // Both window modes reduce a pane to the same render input and the
            // same projected kitty placements, so the GPU code is shared.
            let (input, glyphs, frame_images, block_cards, selected_block) =
                if let (Some(view), Some(remote_panes)) = (remote_view, remote_panes.as_mut()) {
                    let Some(pane_view) = view.pane(layout.pane.0) else {
                        continue;
                    };
                    (
                        pane_view.render_input(),
                        &mut remote_panes.entry(layout.pane).or_default().glyphs,
                        pane_view.placements.clone(),
                        Vec::new(),
                        None,
                    )
                } else {
                    let Some(pane) = panes.get_mut(&layout.pane) else {
                        continue;
                    };
                    let input = PaneRenderInput::from_model(&pane.model);
                    let images = frame_placements(&pane.graphics.placements, &pane.model);
                    let block_cards = if up_cards {
                        nulix::block_cards(&pane.model, input.rows.len())
                    } else {
                        Vec::new()
                    };
                    let selected = pane.model.selected_block_id();
                    (input, &mut pane.glyphs, images, block_cards, selected)
                };
            let palette = &nulix::config::current().palette;
            let cells = input.render_cells(palette);
            // The whole pane around the grid; equal to `layout.rect` in classic mode.
            let frame_rect = chrome::pane_frame_rect(self.ui_mode, layout.rect, layout_cell_width);
            gpu.card_vertices.clear();
            let mut meta_buffers = Vec::new();
            for card in &block_cards {
                let card = cards::trim_blank_tail(*card, |row| {
                    cells
                        .get(row)
                        .is_none_or(|line| line.iter().all(|cell| cell.character == ' '))
                });
                cards::push_card_vertices(
                    &mut gpu.card_vertices,
                    cards::card_rect(&card, layout.rect, gpu.cell_height, layout_cell_width / 2),
                    cards::card_style(&card, selected_block == Some(card.block_id)),
                    gpu.config.width as f32,
                    gpu.config.height as f32,
                );
                let parts = cards::card_meta(&card, now);
                let Some(row) = card.prompt_row.filter(|_| !parts.is_empty()) else {
                    continue;
                };
                let characters: Vec<char> = cells[row].iter().map(|cell| cell.character).collect();
                let Some(mut column) = cards::meta_column(&characters, cards::meta_width(&parts))
                else {
                    continue;
                };
                for part in parts {
                    let columns = part.text.chars().count();
                    let mut buffer = Buffer::new(
                        &mut gpu.font_system,
                        Metrics::new(gpu.font_size, gpu.cell_height),
                    );
                    buffer.set_size(
                        Some(gpu.cell_width * (columns + 1) as f32),
                        Some(gpu.cell_height),
                    );
                    let [r, g, b] = part.color;
                    buffer.set_text(
                        &part.text,
                        &Attrs::new()
                            .family(Family::Monospace)
                            .color(Color::rgb(r, g, b)),
                        cosmic_text::Shaping::Advanced,
                        None,
                    );
                    buffer.shape_until_scroll(&mut gpu.font_system, true);
                    meta_buffers.push((
                        buffer,
                        layout.rect.x as f32 + column as f32 * gpu.cell_width,
                        layout.rect.y as f32 + row as f32 * gpu.cell_height,
                        part.color,
                    ));
                    column += columns;
                }
            }
            gpu.vertices.clear();
            let width = gpu.config.width as f32;
            let height = gpu.config.height as f32;
            for (row, line) in cells.iter().enumerate() {
                for (column, cell) in line.iter().enumerate() {
                    // In up mode default-background cells leave the card fill
                    // (or the cleared pane background) visible.
                    if up_cards && cell.background == palette.background {
                        continue;
                    }
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
            // Up mode draws cards instead of the classic block markers.
            let decorations: &[nulix::renderer::BlockDecoration] =
                if up_cards { &[] } else { &input.decorations };
            for decoration in decorations {
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
                let x = frame_rect.x as f32;
                let y = frame_rect.y as f32;
                let right = (x + frame_rect.width as f32).min(width);
                let bottom = (y + frame_rect.height as f32).min(height);
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
            collect_glyph_positions(&cells, &mut glyphs.positions);
            glyphs.keys.resize(glyphs.positions.len(), None);
            for (index, &(row, column)) in glyphs.positions.iter().enumerate() {
                let cell = cells[row][column];
                if index == glyphs.buffers.len() {
                    glyphs.buffers.push(Buffer::new(
                        &mut gpu.font_system,
                        Metrics::new(gpu.font_size, gpu.cell_height),
                    ));
                }
                let width_cells = unicode_cell_width(cell.character).max(1);
                let key = (cell.character, cell.foreground, cell.bold, width_cells);
                if !glyph_key_requires_reshape(glyphs.keys[index], Some(key)) {
                    continue;
                }
                let buffer = &mut glyphs.buffers[index];
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
                glyphs.keys[index] = Some(key);
            }
            gpu.viewport.update(
                &gpu.queue,
                Resolution {
                    width: gpu.config.width,
                    height: gpu.config.height,
                },
            );
            let pane_bounds = TextBounds {
                left: layout.rect.x as i32,
                top: layout.rect.y as i32,
                right: layout.rect.right().min(gpu.config.width) as i32,
                bottom: layout.rect.bottom().min(gpu.config.height) as i32,
            };
            let areas: Vec<_> = glyphs
                .positions
                .iter()
                .enumerate()
                .map(|(index, &(row, column))| TextArea {
                    buffer: &glyphs.buffers[index],
                    left: layout.rect.x as f32 + column as f32 * gpu.cell_width,
                    top: layout.rect.y as f32 + row as f32 * gpu.cell_height,
                    scale: 1.0,
                    bounds: pane_bounds,
                    default_color: Color::rgb(220, 220, 220),
                    custom_glyphs: &[],
                })
                .chain(
                    meta_buffers
                        .iter()
                        .map(|(buffer, left, top, [r, g, b])| TextArea {
                            buffer,
                            left: *left,
                            top: *top,
                            scale: 1.0,
                            bounds: pane_bounds,
                            default_color: Color::rgb(*r, *g, *b),
                            custom_glyphs: &[],
                        }),
                )
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
            // Placements arrive sorted by z and placement order, so the list
            // position breaks z ties exactly like the placement insertion order.
            let layer_items: Vec<_> = frame_images
                .iter()
                .enumerate()
                .map(|(index, placement)| nulix::renderer::ImageLayerItem {
                    z: placement.z,
                    insertion: index as u64,
                })
                .collect();
            let buckets = nulix::renderer::bucket_image_layers(&layer_items);
            gpu.image_vertices.clear();
            let mut draw_layer = |indices: &[usize]| -> Vec<TextureDraw> {
                let mut draws = Vec::new();
                for &index in indices {
                    let placement = &frame_images[index];
                    let key = (layout.pane, placement.image);
                    // Images still in flight (client mode) are skipped until complete.
                    let Some(image) = pane_image(remote_view, graphics_store, key.0, key.1) else {
                        continue;
                    };
                    if !gpu.textures.contains_key(&key) {
                        continue;
                    }
                    let quad =
                        nulix::renderer::project_image_quad(nulix::renderer::ImageQuadInput {
                            pane: nulix::renderer::PixelRect {
                                x: layout.rect.x as f32,
                                y: layout.rect.y as f32,
                                width: layout.rect.width as f32,
                                height: layout.rect.height as f32,
                            },
                            cell: (gpu.cell_width, gpu.cell_height),
                            anchor: (
                                placement.column.min(i32::MAX as usize) as i32,
                                placement.row,
                            ),
                            cells: (placement.columns, placement.rows),
                            pixel_offset: (placement.x_offset, placement.y_offset),
                            source: (
                                placement.source_x,
                                placement.source_y,
                                placement.source_width,
                                placement.source_height,
                            ),
                            image: (image.width, image.height),
                        });
                    if let Some(quad) = quad {
                        let start = gpu.image_vertices.len() as u32;
                        append_image_quad_vertices(&mut gpu.image_vertices, quad, width, height);
                        draws.push(TextureDraw { key, start });
                    }
                }
                draws
            };
            let under_background_draws = draw_layer(&buckets.under_background);
            let under_text_draws = draw_layer(&buckets.under_text);
            let over_text_draws = draw_layer(&buckets.over_text);
            drop(draw_layer);
            let image_required = gpu.image_vertices.len() * std::mem::size_of::<ImageVertex>();
            if image_required > gpu.image_vertex_buffer_capacity_bytes {
                gpu.image_vertex_buffer_capacity_bytes =
                    nulix::renderer::next_vertex_buffer_capacity_bytes(
                        gpu.image_vertex_buffer_capacity_bytes,
                        image_required,
                    );
                gpu.image_vertex_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("kitty image quad vertex buffer"),
                    size: gpu.image_vertex_buffer_capacity_bytes as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
            }
            if image_required > 0 {
                gpu.queue.write_buffer(
                    &gpu.image_vertex_buffer,
                    0,
                    bytemuck::cast_slice(&gpu.image_vertices),
                );
            }
            gpu.upload_card_vertices();
            let required = gpu.vertices.len() * std::mem::size_of::<RectVertex>();
            let capacity = nulix::renderer::next_vertex_buffer_capacity_bytes(
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
                    label: Some("nulix pane renderer"),
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
                                    r: nulix::config::current().palette.clear_color()[0],
                                    g: nulix::config::current().palette.clear_color()[1],
                                    b: nulix::config::current().palette.clear_color()[2],
                                    a: nulix::config::current().palette.clear_color()[3],
                                })
                            },
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                let scissor = |pass: &mut wgpu::RenderPass<'_>, rect: Rect| {
                    pass.set_scissor_rect(
                        rect.x,
                        rect.y,
                        rect.width.min(gpu.config.width.saturating_sub(rect.x)),
                        rect.height.min(gpu.config.height.saturating_sub(rect.y)),
                    );
                };
                // Cards go first, over the pane background; they reach into
                // the grid padding, so they are clipped to the whole pane.
                if !gpu.card_vertices.is_empty() {
                    scissor(&mut pass, frame_rect);
                    pass.set_pipeline(&gpu.card_pipeline);
                    pass.set_vertex_buffer(0, gpu.card_vertex_buffer.slice(..));
                    pass.draw(0..gpu.card_vertices.len() as u32, 0..1);
                }
                scissor(&mut pass, layout.rect);
                render_texture_draws(
                    &mut pass,
                    &gpu.image_pipeline,
                    &gpu.image_vertex_buffer,
                    &gpu.textures,
                    &under_background_draws,
                );
                if background_vertices > 0 {
                    pass.set_pipeline(&gpu.rect_pipeline);
                    pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
                    pass.draw(0..background_vertices as u32, 0..1);
                }
                render_texture_draws(
                    &mut pass,
                    &gpu.image_pipeline,
                    &gpu.image_vertex_buffer,
                    &gpu.textures,
                    &under_text_draws,
                );
                gpu.text_renderer
                    .render(&gpu.atlas, &gpu.viewport, &mut pass)
                    .map_err(|error| format!("could not render pane text: {error}"))?;
                render_texture_draws(
                    &mut pass,
                    &gpu.image_pipeline,
                    &gpu.image_vertex_buffer,
                    &gpu.textures,
                    &over_text_draws,
                );
                if let Some(range) = overlay_vertex_range(background_vertices, gpu.vertices.len()) {
                    // The focus border follows the whole pane.
                    scissor(&mut pass, frame_rect);
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
                    label: Some("nulix empty surface"),
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
                                r: nulix::config::current().palette.clear_color()[0],
                                g: nulix::config::current().palette.clear_color()[1],
                                b: nulix::config::current().palette.clear_color()[2],
                                a: nulix::config::current().palette.clear_color()[3],
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
                let capacity = nulix::renderer::next_vertex_buffer_capacity_bytes(
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
                    label: Some("nulix pane dividers"),
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
        // Window chrome: the classic tab bar (several tabs only) or `nulix up`'s bars.
        let (chrome_rects, chrome_texts) = match self.ui_mode {
            UiMode::Up => chrome::up_overlay(&chrome::UpChrome {
                labels: &tab_labels,
                active: active_tab_index,
                activity: focused_activity,
                macos: cfg!(target_os = "macos"),
                width: gpu.config.width,
                height: gpu.config.height,
                cell_width: gpu.cell_width,
                cell_height: gpu.cell_height,
            }),
            UiMode::Classic if tab_count > 1 && tab_bar_height > 0 => classic_tab_bar_overlay(
                &tab_labels,
                active_tab_index,
                gpu.config.width,
                tab_bar_height,
            ),
            UiMode::Classic => (Vec::new(), Vec::new()),
        };
        if !chrome_rects.is_empty() || !chrome_texts.is_empty() {
            gpu.vertices.clear();
            for rect in &chrome_rects {
                push_overlay_rect(
                    &mut gpu.vertices,
                    rect.x0,
                    rect.y0,
                    rect.x1,
                    rect.y1,
                    gpu.config.width as f32,
                    gpu.config.height as f32,
                    rect.color,
                );
            }
            let required = gpu.vertices.len() * std::mem::size_of::<RectVertex>();
            if required > gpu.vertex_buffer_capacity_bytes {
                let capacity = nulix::renderer::next_vertex_buffer_capacity_bytes(
                    gpu.vertex_buffer_capacity_bytes,
                    required,
                );
                gpu.vertex_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("terminal tab-bar vertex buffer"),
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
            gpu.prepare_overlay_texts(&chrome_texts)
                .map_err(|error| format!("could not prepare tab labels: {error}"))?;
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("nulix tab bar"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tab bar background and labels"),
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
                pass.draw(0..gpu.vertices.len() as u32, 0..1);
                gpu.text_renderer
                    .render(&gpu.atlas, &gpu.viewport, &mut pass)
                    .map_err(|error| format!("could not render tab labels: {error}"))?;
            }
            gpu.queue.submit([encoder.finish()]);
        }
        if self.help_open && self.ui_mode == UiMode::Up {
            draw_help_overlay(gpu, &view)?;
        }
        gpu.queue.present(frame);
        Ok(FrameRecovery::WaitForEvent)
    }
    fn receive_output(&mut self, pane_id: PaneId, event_loop: &ActiveEventLoop) {
        let mut changed = false;
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            for bytes in pane.pty.drain_output() {
                changed = true;
                pane.graphics.receive(&bytes);
            }
        }
        self.apply_segments(pane_id);
        if self.focused_pane_id() == Some(pane_id) {
            self.refresh_window_title();
        }
        self.poll_child_exit(event_loop);
        if changed
            && self
                .tabs
                .as_ref()
                .is_some_and(|tabs| pane_is_in_active_tab(tabs, pane_id))
        {
            self.request_redraw();
        }
    }

    fn apply_segments(&mut self, pane_id: PaneId) {
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        let mut host = WindowGraphicsHost {
            gpu: self.gpu.as_mut(),
            proxy: &self.proxy,
        };
        pane.apply_output(pane_id, &mut self.graphics, &mut host);
    }

    fn finish_graphics(
        &mut self,
        pane_id: PaneId,
        anchor: nulix::blocks::GridAnchor,
        completion: GraphicsCompletion,
    ) {
        let mut host = WindowGraphicsHost {
            gpu: self.gpu.as_mut(),
            proxy: &self.proxy,
        };
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return;
        };
        let Pane {
            pty,
            model,
            graphics,
            ..
        } = pane;
        graphics.finish_decode(
            pane_id,
            anchor,
            completion,
            model,
            &mut self.graphics,
            &mut host,
            &mut |reply| {
                let _ = pty.write(reply);
            },
        );
        for (evicted_pane, image_id) in self.graphics.take_evicted(&mut host) {
            if let Some(pane) = self.panes.get_mut(&evicted_pane) {
                pane.graphics
                    .forget_evicted(evicted_pane, image_id, &self.graphics.store);
            }
        }
    }
}
impl ApplicationHandler<UserEvent> for TerminalWindow {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.failed {
            return;
        }
        if let Err(e) = self.initialize(event_loop) {
            eprintln!("nulix: {e}");
            self.failed = true;
            event_loop.exit();
            return;
        }
        self.request_redraw();
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyOutput(pane_id) => {
                if let Some(pane) = self.panes.get(&pane_id) {
                    pane.wakeup.acknowledge();
                }
                self.receive_output(pane_id, event_loop);
            }
            UserEvent::GraphicsDecoded(pane_id, anchor, completion) => {
                self.finish_graphics(pane_id, anchor, completion);
                self.apply_segments(pane_id);
                if self.focused_pane_id() == Some(pane_id) {
                    self.refresh_window_title();
                }
                self.request_redraw();
            }
            UserEvent::Control(request, reply) => {
                let response = self.control_command(request, event_loop);
                let _ = reply.send(response);
            }
            UserEvent::Remote(event) => self.remote_event(event, event_loop),
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if let Some(remote) = self.remote.as_mut() {
                    // Detach leaves the session and its shells running on the server.
                    remote.closing = true;
                    if let Some(connection) = &remote.connection {
                        let _ = connection.detach();
                    }
                }
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
                        eprintln!("nulix: {e}");
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
                if self.help_open {
                    // The help swallows typed input like any other key.
                } else if let Some(modes) = self.focused_modes() {
                    let bytes = encode_ime_commit_with_flags(&text, modes.keyboard_flags);
                    let _ = self.write_focused(&bytes);
                    self.scroll_focused_to_bottom();
                }
                self.request_redraw();
                self.ime_gate.commit();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Released => {
                if self.consumed_keys.suppress_release(event.physical_key) {
                    return;
                }
                let flags = self
                    .focused_modes()
                    .map_or(KeyboardFlags::empty(), |modes| modes.keyboard_flags);
                if !flags.contains(KeyboardFlags::REPORT_EVENT_TYPES) {
                    return;
                }
                let key = key_from_winit(
                    &event.logical_key,
                    event.text.as_deref(),
                    event.physical_key,
                    !flags.is_empty(),
                );
                let modifiers = key_modifiers(self.modifiers, self.caps_lock, self.num_lock);
                if let Some(mut input) = key
                    && let Some(modes) = self.focused_modes()
                {
                    input.associated_text = event.text.as_deref();
                    let modifiers =
                        modifier_event_state(modifiers, input.key, KeyEventKind::Release);
                    if let Some(bytes) = encode_key_with_flags(
                        input,
                        modifiers,
                        modes.app_cursor,
                        flags,
                        KeyEventKind::Release,
                    ) {
                        let _ = self.write_focused(&bytes);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if self.consumed_keys.suppress_press(event.physical_key) {
                    return;
                }
                if !event.repeat {
                    match &event.logical_key {
                        Key::Named(NamedKey::CapsLock) => self.caps_lock = !self.caps_lock,
                        Key::Named(NamedKey::NumLock) => self.num_lock = !self.num_lock,
                        _ => {}
                    }
                }
                let mods = key_modifiers(self.modifiers, self.caps_lock, self.num_lock);
                let help_route = help::route_help_key(
                    self.ui_mode,
                    self.help_open,
                    self.focused_modes()
                        .is_some_and(|modes| modes.alternate_screen),
                    help::help_key(&event.logical_key, mods),
                );
                match help_route {
                    HelpRoute::Open | HelpRoute::Close => {
                        self.help_open = help_route == HelpRoute::Open;
                        self.consumed_keys.mark(event.physical_key);
                        self.request_redraw();
                        return;
                    }
                    HelpRoute::Swallow => {
                        self.consumed_keys.mark(event.physical_key);
                        return;
                    }
                    HelpRoute::PassThrough => {}
                }
                let tab_count = self.tab_count();
                let active_tab_index = self.active_tab_index();
                if let Some(action) = tab_shortcut(
                    &event.logical_key,
                    mods,
                    cfg!(target_os = "macos"),
                    tab_count,
                    active_tab_index,
                ) {
                    self.apply_tab_action(action, event_loop);
                    self.consumed_keys.mark(event.physical_key);
                    return;
                }
                let action = block_shortcut(&event.logical_key, mods, cfg!(target_os = "macos"));
                let action = route_block_shortcut(
                    action,
                    self.focused_modes()
                        .is_some_and(|modes| modes.alternate_screen),
                );
                if let Some(action) = action {
                    self.handle_block_shortcut(action);
                    self.consumed_keys.mark(event.physical_key);
                    return;
                }
                match pane_action(&event.logical_key, mods, cfg!(target_os = "macos")) {
                    Some(PaneAction::SplitRight) => {
                        if let Err(error) = self.split_focused(Direction::Horizontal) {
                            eprintln!("nulix: {error}");
                        }
                        self.consumed_keys.mark(event.physical_key);
                        return;
                    }
                    Some(PaneAction::SplitDown) => {
                        if let Err(error) = self.split_focused(Direction::Vertical) {
                            eprintln!("nulix: {error}");
                        }
                        self.consumed_keys.mark(event.physical_key);
                        return;
                    }
                    Some(PaneAction::Close) => {
                        self.close_focused(event_loop);
                        self.consumed_keys.mark(event.physical_key);
                        return;
                    }
                    Some(PaneAction::Focus(direction)) => {
                        self.focus_direction(direction);
                        self.consumed_keys.mark(event.physical_key);
                        return;
                    }
                    None => {}
                }
                let kitty_flags = self
                    .focused_modes()
                    .map_or(KeyboardFlags::empty(), |modes| modes.keyboard_flags);
                let kitty_active = kitty_flags.intersects(
                    KeyboardFlags::DISAMBIGUATE
                        | KeyboardFlags::REPORT_EVENT_TYPES
                        | KeyboardFlags::REPORT_ALL,
                );
                let terminal_input = key_from_winit(
                    &event.logical_key,
                    event.text.as_deref(),
                    event.physical_key,
                    kitty_active,
                );
                let terminal_key = terminal_input.map(|input| input.key);
                if let Some(TerminalKey::Text(text)) = terminal_key {
                    let clipboard_shortcut = (mods.control && mods.shift)
                        || (cfg!(target_os = "macos") && mods.super_key);
                    if clipboard_shortcut && text.eq_ignore_ascii_case("c") {
                        self.copy_selection();
                        self.consumed_keys.mark(event.physical_key);
                        return;
                    }
                    if clipboard_shortcut && text.eq_ignore_ascii_case("v") {
                        self.paste_clipboard();
                        self.consumed_keys.mark(event.physical_key);
                        return;
                    }
                }
                let suppress_key =
                    terminal_key.is_some_and(|key| self.ime_gate.suppresses_key(key, mods.control));
                if let (Some(key), Some(modes)) = (terminal_key, self.focused_modes())
                    && !suppress_key
                {
                    let app_cursor = modes.app_cursor;
                    let mut input = terminal_input.unwrap_or_else(|| KeyInput::new(key));
                    input.associated_text = event.text.as_deref();
                    let kind = if event.repeat {
                        KeyEventKind::Repeat
                    } else {
                        KeyEventKind::Press
                    };
                    let protocol_modifiers = modifier_event_state(mods, key, kind);
                    if let Some(bytes) = encode_key_with_flags(
                        input,
                        protocol_modifiers,
                        app_cursor,
                        modes.keyboard_flags,
                        kind,
                    ) {
                        let _ = self.write_focused(&bytes);
                    }
                    self.scroll_focused_to_bottom();
                }
                self.request_redraw();
            }
            WindowEvent::Ime(winit::event::Ime::Enabled) => {}
            WindowEvent::Ime(winit::event::Ime::Disabled) => self.ime_gate.disable(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position = position;
                let endpoint = if let Some(origin) = self.selection_origin {
                    let gpu = self.gpu.as_ref();
                    gpu.and_then(|gpu| {
                        pane_cell_for_id(
                            position.x,
                            position.y,
                            origin,
                            &self.pane_layouts(),
                            gpu.cell_width,
                            gpu.cell_height,
                        )
                    })
                } else {
                    self.pane_and_cell_at_cursor()
                };
                if let Some((pane_id, column, row)) = endpoint
                    && let Some(router) = self.selection_router_mut(pane_id)
                    && router.cursor_moved(Some((column, row))) == PointerAction::Extend
                {
                    self.selection_op(pane_id, SelectionOp::Extend { column, row });
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
                if self.help_open && state == PointerState::Pressed {
                    // A click anywhere closes the help and does nothing else.
                    self.help_open = false;
                    self.request_redraw();
                    return;
                }
                if state == PointerState::Pressed
                    && button == PointerButton::Primary
                    && self.up_chrome_click(event_loop)
                {
                    return;
                }
                if state == PointerState::Pressed && button == PointerButton::Primary {
                    let tab_count = self.tab_count();
                    if let Some(remote) = self.remote.as_mut()
                        && let Some(gpu) = self.gpu.as_ref()
                        && self.cursor_position.y >= 0.0
                        && self.cursor_position.y
                            < f64::from(tab_bar_height(tab_count, gpu.cell_height))
                        && let Some(index) =
                            tab_index_at(self.cursor_position.x, gpu.config.width, tab_count)
                    {
                        remote.layout(LayoutAction::SelectTab { index });
                        return;
                    }
                    let bar_height = self
                        .gpu
                        .as_ref()
                        .map_or(0, |gpu| tab_bar_height(tab_count, gpu.cell_height));
                    if self.cursor_position.y >= 0.0
                        && self.cursor_position.y < f64::from(bar_height)
                        && let Some(gpu) = self.gpu.as_ref()
                        && let Some(index) =
                            tab_index_at(self.cursor_position.x, gpu.config.width, tab_count)
                        && let Some(tabs) = self.tabs.as_mut()
                    {
                        tabs.select(index);
                        self.refresh_window_title();
                        self.request_redraw();
                        return;
                    }
                }
                let hit = self.pane_and_cell_at_cursor();
                if state == PointerState::Pressed
                    && button == PointerButton::Primary
                    && let Some(previous) = self.selection_origin
                    && Some(previous) != hit.map(|(pane_id, _, _)| pane_id)
                    && let Some(router) = self.selection_router_mut(previous)
                {
                    let _ = router.mouse_input(
                        PointerButton::Primary,
                        PointerState::Released,
                        None,
                        Instant::now(),
                    );
                }
                if state == PointerState::Pressed
                    && let Some((pane_id, _, _)) = hit
                    && self.focused_pane_id() != Some(pane_id)
                    && let Some(remote) = self.remote.as_mut()
                {
                    remote.layout(LayoutAction::FocusPane { pane: pane_id.0 });
                }
                if state == PointerState::Pressed
                    && let Some((pane_id, _, _)) = hit
                    && let Some(tab) = self.tabs.as_mut().and_then(TabSet::active_mut)
                {
                    let _ = tab.focus_by_click(
                        self.cursor_position.x.max(0.0) as u32,
                        self.cursor_position.y.max(0.0) as u32,
                    );
                    debug_assert_eq!(tab.active_pane(), pane_id);
                }
                if state == PointerState::Pressed && button == PointerButton::Primary {
                    self.selection_origin = hit.map(|(pane_id, _, _)| pane_id);
                }
                let route_id =
                    selection_target(self.selection_origin, hit.map(|(id, _, _)| id), state);
                let route_cell = route_id.and_then(|id| {
                    if state == PointerState::Pressed {
                        hit
                    } else {
                        let gpu = self.gpu.as_ref()?;
                        pane_cell_for_id(
                            self.cursor_position.x,
                            self.cursor_position.y,
                            id,
                            &self.pane_layouts(),
                            gpu.cell_width,
                            gpu.cell_height,
                        )
                    }
                });
                let action = route_id
                    .and_then(|pane_id| {
                        let router = self.selection_router_mut(pane_id)?;
                        let cell = route_cell.map(|(_, column, row)| (column, row));
                        Some(router.mouse_input(button, state, cell, Instant::now()))
                    })
                    .unwrap_or(PointerAction::Ignore);
                let pane_id = route_id;
                match action {
                    PointerAction::Begin(gesture) => {
                        if let (Some((_, column, row)), Some(pane_id)) = (route_cell, pane_id) {
                            let kind = match gesture {
                                SelectionGesture::Simple => SelectionType::Simple,
                                SelectionGesture::Semantic => SelectionType::Semantic,
                                SelectionGesture::Lines => SelectionType::Lines,
                            };
                            self.selection_op(pane_id, SelectionOp::Begin { column, row, kind });
                        }
                    }
                    PointerAction::Clear => {
                        if let Some(pane_id) = pane_id {
                            self.selection_op(pane_id, SelectionOp::Clear);
                        }
                    }
                    _ => {}
                }
                if action != PointerAction::Ignore {
                    self.request_redraw();
                }
                if state == PointerState::Released && button == PointerButton::Primary {
                    self.selection_origin = None;
                }
            }
            WindowEvent::MouseWheel { delta, .. } if self.remote.is_some() => {
                self.remote_wheel(delta);
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
                        pane.glyphs.invalidate();
                    }
                    if let Some(remote) = self.remote.as_mut() {
                        for state in remote.panes.values_mut() {
                            state.glyphs.invalidate();
                        }
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
        if let (Some(remote), Some(gpu)) = (self.remote.as_mut(), self.gpu.as_ref())
            && remote.settle.due(now)
        {
            let metrics = WindowMetrics::new(
                gpu.config.width,
                gpu.config.height,
                gpu.cell_width,
                gpu.cell_height,
            );
            if let Err(error) = remote.attach(metrics, self.proxy.clone()) {
                eprintln!("nulix: {error}");
                remote.closing = true;
                self.failed = true;
                event_loop.exit();
                return;
            }
        }
        let any_running = self.pane_layouts().iter().any(|layout| {
            !layout.hidden
                && self
                    .panes
                    .get(&layout.pane)
                    .is_some_and(|pane| pane.model.blocks().activity() == PaneActivity::Running)
        });
        let (redraw, tick) =
            chrome::running_tick(self.ui_mode, any_running, now, self.running_tick_at);
        self.running_tick_at = tick;
        if redraw {
            self.request_redraw();
        }
        let attach_deadline = self
            .remote
            .as_ref()
            .and_then(|remote| remote.settle.deadline());
        let next_deadline = [
            (!self.panes.is_empty()).then_some(self.next_exit_poll),
            self.retry_redraw_at,
            self.running_tick_at,
            attach_deadline,
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

const USAGE: &str = "\
Usage: nulix [--session NAME]
       nulix up
       nulix down

Opens a nulix window running a local shell.

Commands:
  up              Inside a nulix window: switch this window to the friendly
                  UI mode for newcomers and demos. Elsewhere: open a new
                  window in that mode (its pane shells see NULIX_UI=up).
                  F1 shows the shortcut help (Esc or F1 closes it); inside
                  full-screen apps F1 goes to the app.
  down            Inside a nulix window: switch this window back to the
                  classic look. Fails outside a nulix window.

Options:
  --session NAME  Attach the window to the persistent nulix-server session
                  NAME, starting the server when it is not running. NAME is
                  1-64 ASCII letters, digits, '-' or '_'. Closing the window
                  detaches and leaves the session running.
  -h, --help      Print this help.

Configuration is read at startup from $NULIX_CONFIG, else
$XDG_CONFIG_HOME/nulix/nulix.conf (default ~/.config/nulix/nulix.conf):
`key = value` lines for font, font_size, shell, scrollback, foreground,
background, cursor and color0..color15 (#rrggbb). NULIX_FONT,
NULIX_FONT_SIZE and NULIX_SHELL override the file.
";

/// What the command line asks the window to do.
#[derive(Debug, PartialEq, Eq)]
enum WindowArgs {
    Standalone,
    Session(String),
    /// `nulix up`: switch the enclosing window to the friendly UI mode, or
    /// open a new standalone window in it.
    Up,
    /// `nulix down`: switch the enclosing window back to classic.
    Down,
    Help,
}

/// How the enclosing window (`NULIX_SOCKET`) answered a `ui` switch request.
#[derive(Debug, PartialEq, Eq)]
enum SwitchReply {
    Switched,
    /// The window or server answered with an error (its message).
    Refused(String),
    /// No socket, or nothing answers on it (missing or stale socket).
    Unreachable,
}

/// What `nulix up` / `nulix down` do after asking the enclosing window.
#[derive(Debug, PartialEq, Eq)]
enum SwitchAction {
    /// The enclosing window switched; exit 0 silently.
    Done,
    /// Not inside a reachable window: open a new up-mode window.
    OpenUpWindow,
    /// Print `nulix: MESSAGE` and exit 1.
    Fail(String),
}

fn switch_action(target: UiMode, reply: SwitchReply) -> SwitchAction {
    match (target, reply) {
        (_, SwitchReply::Switched) => SwitchAction::Done,
        (_, SwitchReply::Refused(message)) => SwitchAction::Fail(message),
        (UiMode::Up, SwitchReply::Unreachable) => SwitchAction::OpenUpWindow,
        (UiMode::Classic, SwitchReply::Unreachable) => {
            SwitchAction::Fail("down only works inside a nulix window".into())
        }
    }
}

/// The enclosing window's control socket: `NULIX_SOCKET` when non-empty.
fn enclosing_socket(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Upper bound of the `ui` request of `nulix up` / `nulix down`.
#[cfg(unix)]
const UI_SWITCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Asks the window behind `socket` to switch to `mode` through the shared
/// control client. Transport failures count as not being inside a window.
fn request_ui_switch(socket: Option<&std::path::Path>, mode: UiMode) -> SwitchReply {
    let Some(socket) = socket else {
        return SwitchReply::Unreachable;
    };
    #[cfg(unix)]
    {
        let request = ControlRequest {
            id: 1,
            command: ControlCommand::Ui {
                mode: Some(chrome::control_ui_mode(mode)),
            },
        };
        match control::send_request_with_timeout(socket, &request, UI_SWITCH_TIMEOUT) {
            Ok(response) => match response.outcome {
                ControlOutcome::Success(_) => SwitchReply::Switched,
                ControlOutcome::Failure(error) => SwitchReply::Refused(error.message),
            },
            Err(_) => SwitchReply::Unreachable,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, mode);
        SwitchReply::Unreachable
    }
}

/// The `NULIX_UI` value pane shells inherit; `None` removes an inherited one,
/// so a plain window opened from an up-mode pane does not stay in up mode.
fn nulix_ui_env(args: &WindowArgs) -> Option<&'static str> {
    matches!(args, WindowArgs::Up).then_some("up")
}

fn parse_window_args(args: impl IntoIterator<Item = OsString>) -> Result<WindowArgs, String> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(WindowArgs::Standalone);
    };
    // Only `up`, `down`, --session and --help are recognised; anything else is
    // ignored, as the standalone window always did.
    let Ok(first) = first.into_string() else {
        return Ok(WindowArgs::Standalone);
    };
    let session = match first.as_str() {
        "-h" | "--help" => return Ok(WindowArgs::Help),
        "up" | "down" => {
            if let Some(extra) = args.next() {
                return Err(format!("unexpected argument {extra:?}"));
            }
            return Ok(if first == "up" {
                WindowArgs::Up
            } else {
                WindowArgs::Down
            });
        }
        "--session" => args
            .next()
            .ok_or_else(|| "--session requires a session NAME".to_owned())?
            .into_string()
            .map_err(|name| format!("invalid session name {name:?}"))?,
        other => match other.strip_prefix("--session=") {
            Some(name) => name.to_owned(),
            None => return Ok(WindowArgs::Standalone),
        },
    };
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument {extra:?}"));
    }
    control::validate_session_name(&session)?;
    Ok(WindowArgs::Session(session))
}

fn main() {
    let args = parse_window_args(std::env::args_os().skip(1));
    if let Ok(args) = &args {
        // Set before any thread, PTY or server process exists, so every pane
        // shell (and an autostarted nulix-server) inherits the final value.
        match nulix_ui_env(args) {
            Some(value) => unsafe { std::env::set_var("NULIX_UI", value) },
            None => unsafe { std::env::remove_var("NULIX_UI") },
        }
    }
    let code = match args {
        Ok(WindowArgs::Help) => {
            print!("{USAGE}");
            0
        }
        Ok(WindowArgs::Standalone) => {
            nulix::config::init();
            run_terminal(None, UiMode::Classic)
        }
        Ok(WindowArgs::Up) => run_ui_switch(UiMode::Up),
        Ok(WindowArgs::Down) => run_ui_switch(UiMode::Classic),
        Ok(WindowArgs::Session(session)) => {
            // The client applies fonts and colours; the server reads the same
            // file for `shell` and `scrollback`.
            nulix::config::init();
            run_session_client(&session)
        }
        Err(error) => {
            eprintln!("nulix: {error}");
            eprint!("{USAGE}");
            2
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}

/// `nulix up` / `nulix down`: switch the enclosing window in place, or (up
/// only) open a new up-mode window when not inside a reachable one.
fn run_ui_switch(mode: UiMode) -> i32 {
    let socket = enclosing_socket(std::env::var_os("NULIX_SOCKET"));
    match switch_action(mode, request_ui_switch(socket.as_deref(), mode)) {
        SwitchAction::Done => 0,
        SwitchAction::OpenUpWindow => {
            nulix::config::init();
            run_terminal(None, UiMode::Up)
        }
        SwitchAction::Fail(message) => {
            eprintln!("nulix: {message}");
            1
        }
    }
}

/// `nulix --session NAME`: make sure the session server runs, then open the
/// window as its attached client.
fn run_session_client(session: &str) -> i32 {
    if cfg!(not(unix)) {
        eprintln!("nulix: persistent sessions are unsupported on this platform");
        return 1;
    }
    match nulix::remote::ensure_server(session) {
        Ok(socket) => run_terminal(Some(socket), UiMode::Classic),
        Err(error) => {
            eprintln!("nulix: could not reach session {session}: {error}");
            1
        }
    }
}

fn run_terminal(session_socket: Option<PathBuf>, ui_mode: UiMode) -> i32 {
    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("nulix: could not initialize the window event loop: {e}");
            return 1;
        }
    };
    let mut app = TerminalWindow::new(event_loop.create_proxy(), session_socket, ui_mode);
    if let Err(error) = event_loop.run_app(&mut app) {
        eprintln!("nulix: window event loop failed: {error}");
        return 1;
    }
    if app.failed {
        return 1;
    }
    0
}

fn start_control_server(proxy: EventLoopProxy<UserEvent>) -> Option<ControlServer> {
    #[cfg(unix)]
    {
        if std::env::var_os("NULIX_CONTROL").as_deref() == Some(std::ffi::OsStr::new("0")) {
            return None;
        }
        let directory = match control::runtime_directory() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("nulix: unable to resolve control runtime directory: {error}");
                return None;
            }
        };
        let listener_proxy = proxy;
        match ControlServer::start_at(&directory, std::process::id(), move |request| {
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let timeout = ControlResponse {
                id: Some(request.id),
                outcome: ControlOutcome::Failure(ControlError::for_request(
                    Some(request.id),
                    ControlErrorCode::Timeout,
                    "event loop did not answer within 30 seconds",
                )),
            };
            if listener_proxy
                .send_event(UserEvent::Control(request, sender))
                .is_err()
            {
                return ControlResponse {
                    id: timeout.id,
                    outcome: ControlOutcome::Failure(ControlError::for_request(
                        timeout.id,
                        ControlErrorCode::Unavailable,
                        "terminal event loop is unavailable",
                    )),
                };
            }
            receiver
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap_or(timeout)
        }) {
            Ok(server) => Some(server),
            Err(error) => {
                eprintln!("nulix: unable to start local control socket: {error}");
                None
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = proxy;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::UiMode;
    use super::{
        BlockShortcut, ConsumedKeyPresses, GlyphKey, KeyEventKind, KeyboardFlags, PaneAction,
        TabAction, TerminalKey, apply_title_event, block_marker_bounds, block_shortcut,
        close_pane_lifecycle, create_tab_with, cwd_basename_title, encode_key_with_flags,
        glyph_key_requires_reshape, invalidate_glyph_cache, key_from_winit, modifier_event_state,
        overlay_vertex_range, pane_action, pane_cell_at, pane_exit_needs_redraw,
        pane_is_in_active_tab, pane_tab_title, retain_live_textures, route_block_shortcut,
        selection_target, tab_action_changes_geometry, tab_bar_height, tab_content_rect,
        tab_index_at, tab_shortcut, tab_style,
    };
    use nulix::keyboard::Modifiers;
    use nulix::layout::{PaneId, PaneLayout, Rect};
    use nulix::model::{Model, ModelEvent};
    use nulix::selection::PointerState;
    use winit::keyboard::{Key, NamedKey};

    fn window_args(args: &[&str]) -> Result<super::WindowArgs, String> {
        super::parse_window_args(args.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn window_args_select_standalone_session_client_or_help() {
        use super::WindowArgs;
        assert_eq!(window_args(&[]), Ok(super::WindowArgs::Standalone));
        assert_eq!(window_args(&["--help"]), Ok(WindowArgs::Help));
        assert_eq!(window_args(&["-h"]), Ok(WindowArgs::Help));
        assert_eq!(
            window_args(&["--session", "work-1"]),
            Ok(WindowArgs::Session("work-1".into()))
        );
        assert_eq!(
            window_args(&["--session=dev_2"]),
            Ok(WindowArgs::Session("dev_2".into()))
        );
        assert!(super::USAGE.contains("--session NAME"));
    }

    #[test]
    fn window_args_select_up_mode_and_reject_extra_arguments() {
        use super::WindowArgs;
        assert_eq!(window_args(&["up"]), Ok(WindowArgs::Up));
        assert!(
            window_args(&["up", "--session", "demo"])
                .unwrap_err()
                .contains("unexpected argument")
        );
        // Only the exact word selects up mode; other words stay ignored.
        assert_eq!(window_args(&["upx"]), Ok(WindowArgs::Standalone));
        assert!(super::USAGE.contains("nulix up"));
    }

    #[test]
    fn window_args_select_down_and_reject_extra_arguments() {
        use super::WindowArgs;
        assert_eq!(window_args(&["down"]), Ok(WindowArgs::Down));
        assert!(
            window_args(&["down", "now"])
                .unwrap_err()
                .contains("unexpected argument")
        );
        assert_eq!(window_args(&["downx"]), Ok(WindowArgs::Standalone));
        assert!(super::USAGE.contains("nulix down"));
        assert!(super::USAGE.contains("Inside a nulix window"));
    }

    #[test]
    fn in_place_switch_decides_between_done_new_window_and_error() {
        use super::{SwitchAction, SwitchReply, UiMode, switch_action};
        for mode in [UiMode::Up, UiMode::Classic] {
            assert_eq!(
                switch_action(mode, SwitchReply::Switched),
                SwitchAction::Done
            );
            assert_eq!(
                switch_action(mode, SwitchReply::Refused("no".into())),
                SwitchAction::Fail("no".into())
            );
        }
        assert_eq!(
            switch_action(UiMode::Up, SwitchReply::Unreachable),
            SwitchAction::OpenUpWindow
        );
        assert_eq!(
            switch_action(UiMode::Classic, SwitchReply::Unreachable),
            SwitchAction::Fail("down only works inside a nulix window".into())
        );
    }

    #[test]
    fn only_a_non_empty_nulix_socket_names_the_enclosing_window() {
        use std::ffi::OsString;
        assert_eq!(super::enclosing_socket(None), None);
        assert_eq!(super::enclosing_socket(Some(OsString::new())), None);
        assert_eq!(
            super::enclosing_socket(Some(OsString::from("/run/nulix/nulix-1.sock"))),
            Some(std::path::PathBuf::from("/run/nulix/nulix-1.sock"))
        );
        // Without a socket there is nothing to ask.
        assert_eq!(
            super::request_ui_switch(None, UiMode::Up),
            super::SwitchReply::Unreachable
        );
    }

    #[test]
    fn only_up_mode_exports_nulix_ui_to_pane_shells() {
        use super::WindowArgs;
        assert_eq!(super::nulix_ui_env(&WindowArgs::Up), Some("up"));
        assert_eq!(super::nulix_ui_env(&WindowArgs::Standalone), None);
        assert_eq!(
            super::nulix_ui_env(&WindowArgs::Session("demo".into())),
            None
        );
    }

    #[test]
    fn window_args_reject_invalid_session_names_and_ignore_unknown_arguments() {
        let invalid = window_args(&["--session", "bad name"]).unwrap_err();
        assert!(invalid.contains("invalid session name"), "{invalid}");
        assert!(window_args(&["--session", ""]).is_err());
        assert!(window_args(&["--session", &"x".repeat(65)]).is_err());
        assert!(
            window_args(&["--session"])
                .unwrap_err()
                .contains("requires a session NAME")
        );
        // Arguments other than --session/--help were always ignored by the
        // standalone window; keep that behaviour.
        assert_eq!(window_args(&["--bogus"]), Ok(super::WindowArgs::Standalone));
        assert_eq!(
            window_args(&["file.txt", "-x"]),
            Ok(super::WindowArgs::Standalone)
        );
        assert!(
            window_args(&["--session", "a", "b"])
                .unwrap_err()
                .contains("unexpected argument")
        );
    }

    #[test]
    fn deleted_or_quota_evicted_images_drop_their_cached_texture_keys() {
        let mut cached = std::collections::BTreeMap::from([
            ((PaneId(1), 10), "deleted"),
            ((PaneId(2), 20), "quota-evicted"),
            ((PaneId(2), 21), "still-live"),
        ]);
        retain_live_textures(&mut cached, |pane, id| (pane, id) == (PaneId(2), 21));
        assert_eq!(
            cached,
            std::collections::BTreeMap::from([((PaneId(2), 21), "still-live")])
        );
    }

    #[test]
    fn tab_bar_geometry_is_hidden_for_one_tab_and_reserves_one_cell_row_for_many() {
        let window = Rect::new(0, 0, 800, 600);
        assert_eq!(tab_bar_height(1, 20.0), 0);
        assert_eq!(tab_bar_height(2, 20.0), 20);
        assert_eq!(tab_content_rect(window, 1, 20.0), window);
        assert_eq!(
            tab_content_rect(window, 2, 20.0),
            Rect::new(0, 20, 800, 580)
        );
        assert_eq!(tab_index_at(0.0, 800, 2), Some(0));
        assert_eq!(tab_index_at(799.0, 800, 2), Some(1));
        assert_eq!(tab_index_at(800.0, 800, 2), None);
    }

    #[test]
    fn tab_shortcuts_route_new_close_cycle_and_numbered_selection() {
        let control_shift = Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        };
        let control = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            tab_shortcut(&Key::Character("t".into()), control_shift, false, 2, 0),
            Some(TabAction::New)
        );
        assert_eq!(
            tab_shortcut(&Key::Character("q".into()), control_shift, false, 2, 0),
            Some(TabAction::Close)
        );
        assert_eq!(
            tab_shortcut(&Key::Named(NamedKey::Tab), control, false, 2, 0),
            Some(TabAction::Next)
        );
        assert_eq!(
            tab_shortcut(&Key::Named(NamedKey::Tab), control_shift, false, 2, 0),
            Some(TabAction::Previous)
        );
        assert_eq!(
            tab_shortcut(&Key::Character("2".into()), alt, false, 3, 0),
            Some(TabAction::Select(1))
        );
        assert_eq!(
            tab_shortcut(&Key::Character("2".into()), alt, false, 3, 1),
            None,
            "Alt+2 must reach the shell when tab 2 is already active"
        );
        assert_eq!(
            tab_shortcut(&Key::Character("9".into()), alt, false, 2, 0),
            None
        );
        assert_eq!(
            tab_shortcut(&Key::Character("1".into()), alt, false, 1, 0),
            None,
            "Alt+1 must reach the shell when it would select the current tab"
        );
        assert_eq!(
            tab_shortcut(
                &Key::Character("2".into()),
                Modifiers {
                    super_key: true,
                    ..alt
                },
                true,
                3,
                0
            ),
            Some(TabAction::Select(1))
        );
    }

    #[test]
    fn winit_key_input_keeps_unshifted_and_physical_alternate_codes() {
        let logical = Key::Character("A".into());
        let input = key_from_winit(
            &logical,
            Some("A"),
            winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyA),
            true,
        )
        .unwrap();
        assert_eq!(input.key_code, Some(u32::from('a')));
        assert_eq!(input.shifted_key_code, Some(u32::from('A')));
        assert_eq!(input.base_layout_key_code, Some(u32::from('a')));

        let logical = Key::Character("Ф".into());
        let input = key_from_winit(
            &logical,
            Some("Ф"),
            winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyA),
            true,
        )
        .unwrap();
        assert_eq!(input.key_code, Some(u32::from('ф')));
        assert_eq!(input.base_layout_key_code, Some(u32::from('a')));
    }

    #[test]
    fn named_text_keys_keep_legacy_and_kitty_text() {
        let physical = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Space);
        let encode = |input, flags| {
            encode_key_with_flags(
                input,
                Modifiers::default(),
                false,
                flags,
                KeyEventKind::Press,
            )
            .unwrap()
        };
        let space = Key::Named(NamedKey::Space);
        let legacy = key_from_winit(&space, Some(" "), physical, false).unwrap();
        assert_eq!(encode(legacy, KeyboardFlags::empty()), b" ");

        let kitty = key_from_winit(&space, Some(" "), physical, true).unwrap();
        assert_eq!(kitty.key_code, Some(32));
        assert_eq!(encode(kitty, KeyboardFlags::DISAMBIGUATE), b" ");
        assert_eq!(encode(kitty, KeyboardFlags::REPORT_ALL), b"\x1b[32u");

        let unmapped = Key::Named(NamedKey::Help);
        let input = key_from_winit(
            &unmapped,
            Some("z"),
            winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyZ),
            true,
        )
        .unwrap();
        assert_eq!(input.key, TerminalKey::Text("z"));
        assert_eq!(encode(input, KeyboardFlags::REPORT_ALL), b"\x1b[122u");
    }

    #[test]
    fn keypad_navigation_uses_keypad_codes_only_for_non_text_kitty_events() {
        let logical = Key::Character("4".into());
        let legacy = key_from_winit(
            &logical,
            Some("4"),
            winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Numpad4),
            false,
        )
        .unwrap();
        assert_eq!(legacy.key, TerminalKey::Text("4"));

        let navigation = Key::Named(NamedKey::Home);
        let kitty = key_from_winit(
            &navigation,
            None,
            winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Numpad7),
            true,
        )
        .unwrap();
        assert_eq!(
            kitty.key,
            TerminalKey::NumpadKey(nulix::keyboard::KeypadKey::Home)
        );
    }

    #[test]
    fn numpad_navigation_does_not_become_a_digit_when_numlock_is_off() {
        let physical = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Numpad7);
        let home = Key::Named(NamedKey::Home);
        let legacy = key_from_winit(&home, None, physical, false).unwrap();
        assert_eq!(legacy.key, TerminalKey::Home);
        let kitty = key_from_winit(&home, None, physical, true).unwrap();
        assert_eq!(
            kitty.key,
            TerminalKey::NumpadKey(nulix::keyboard::KeypadKey::Home)
        );
    }

    #[test]
    fn consumed_shortcut_press_suppresses_repeat_and_release() {
        let key = winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyC);
        let mut consumed = ConsumedKeyPresses::default();
        consumed.mark(key);
        assert!(consumed.suppress_press(key));
        assert!(consumed.suppress_press(key), "repeat remains consumed");
        assert!(consumed.suppress_release(key));
        assert!(!consumed.suppress_press(key));
        assert!(!consumed.suppress_release(key));
    }

    #[test]
    fn modifier_key_event_state_includes_press_and_release_effect() {
        let modifiers = Modifiers::default();
        let pressed = modifier_event_state(
            modifiers,
            TerminalKey::Modifier(nulix::keyboard::ModifierKey::LeftControl),
            KeyEventKind::Press,
        );
        assert!(pressed.control);
        let released = modifier_event_state(
            pressed,
            TerminalKey::Modifier(nulix::keyboard::ModifierKey::LeftControl),
            KeyEventKind::Release,
        );
        assert!(!released.control);
    }

    #[test]
    fn failed_new_tab_spawn_rolls_back_the_empty_tab() {
        use nulix::layout::{CellSize, TabSet};
        let mut tabs = TabSet::new(Rect::new(0, 0, 800, 600), CellSize::new(10, 10), 4, 5, 3);
        let original = tabs.active().unwrap().id();
        let mut spawn_attempted = false;
        let result = create_tab_with(&mut tabs, |_, _| {
            spawn_attempted = true;
            Err::<(), _>("injected PTY spawn failure".to_owned())
        });
        assert!(spawn_attempted);
        assert_eq!(result, Err("injected PTY spawn failure".to_owned()));
        assert_eq!(tabs.tabs().len(), 1);
        assert_eq!(tabs.active().unwrap().id(), original);
    }

    #[test]
    fn tab_switch_actions_do_not_request_pty_or_model_resize() {
        assert!(!tab_action_changes_geometry(TabAction::Next));
        assert!(!tab_action_changes_geometry(TabAction::Previous));
        assert!(!tab_action_changes_geometry(TabAction::Select(1)));
        assert!(tab_action_changes_geometry(TabAction::New));
        assert!(tab_action_changes_geometry(TabAction::Close));
    }

    #[test]
    fn cwd_title_uses_the_basename_and_explicit_title_takes_precedence() {
        let cwd = std::path::Path::new("/tmp/project");
        assert_eq!(cwd_basename_title(cwd), "project");
        assert_eq!(pane_tab_title(cwd, None), "project");
        assert_eq!(pane_tab_title(cwd, Some("custom title")), "custom title");
        assert_eq!(pane_tab_title(cwd, None), "project", "reset returns to cwd");
        assert_eq!(cwd_basename_title(std::path::Path::new("/")), "shellux");
        let mut explicit_title = None;
        assert_eq!(
            apply_title_event(
                cwd,
                &mut explicit_title,
                &ModelEvent::Title("hello".to_owned())
            ),
            Some("hello".to_owned())
        );
        assert_eq!(
            apply_title_event(cwd, &mut explicit_title, &ModelEvent::ResetTitle),
            Some("project".to_owned())
        );
        let mut model = Model::new(20, 3);
        model.feed(b"\x1b]2;hello\x07");
        assert!(
            model
                .take_events()
                .contains(&ModelEvent::Title("hello".into()))
        );
    }

    #[test]
    fn classic_tab_bar_overlay_keeps_equal_width_tabs_and_tab_styles() {
        let labels = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let (rects, texts) = super::classic_tab_bar_overlay(&labels, 1, 900, 20);
        let spans: Vec<_> = rects
            .iter()
            .map(|rect| (rect.x0, rect.x1, rect.y1))
            .collect();
        assert_eq!(
            spans,
            vec![
                (0.0, 300.0, 20.0),
                (300.0, 600.0, 20.0),
                (600.0, 900.0, 20.0)
            ]
        );
        assert_eq!(rects[1].color, tab_style(true).0);
        assert_eq!(rects[0].color, tab_style(false).0);
        assert_eq!((texts[1].left, texts[1].width), (305.0, 290.0));
        assert_eq!(texts[1].color, tab_style(true).1);
        assert_eq!(texts[2].clip, [0, 0, 900, 20]);
    }

    #[test]
    fn active_tab_style_has_clearer_contrast_than_inactive_style() {
        let active = tab_style(true);
        let inactive = tab_style(false);
        assert_ne!(active.0, inactive.0);
        assert_eq!(active.1, [255, 255, 255]);
        assert!(inactive.1[0] < active.1[0]);
    }

    #[test]
    fn drag_release_routes_to_the_pane_that_started_selection() {
        let origin = PaneId(11);
        let other = PaneId(12);
        assert_eq!(
            selection_target(None, Some(origin), PointerState::Pressed),
            Some(origin)
        );
        assert_eq!(
            selection_target(Some(origin), Some(other), PointerState::Released),
            Some(origin)
        );
        assert_eq!(
            selection_target(None, Some(other), PointerState::Released),
            None
        );
    }

    #[test]
    fn hidden_tab_output_does_not_request_a_redraw_for_the_active_tab() {
        use nulix::layout::{CellSize, TabSet};
        let mut tabs = TabSet::new(Rect::new(0, 0, 800, 600), CellSize::new(10, 10), 4, 5, 3);
        let first = tabs.active().unwrap().active_pane();
        tabs.add_tab();
        assert_ne!(first, tabs.active().unwrap().active_pane());
        assert!(pane_is_in_active_tab(
            &tabs,
            tabs.active().unwrap().active_pane()
        ));
        assert!(!pane_is_in_active_tab(&tabs, first));
    }

    #[test]
    fn hidden_tab_exit_only_redraws_when_the_visible_tab_bar_changes() {
        assert!(!pane_exit_needs_redraw(false, 2, 2));
        assert!(pane_exit_needs_redraw(false, 2, 1));
        assert!(pane_exit_needs_redraw(true, 2, 2));
    }

    #[test]
    fn pane_exit_uses_the_same_close_lifecycle_and_returns_resized_siblings() {
        use nulix::layout::{CellSize, Direction, TabSet};
        let mut tabs = TabSet::new(Rect::new(0, 0, 1200, 600), CellSize::new(10, 10), 4, 5, 3);
        let left = tabs.active().unwrap().active_pane();
        let right = tabs
            .active_mut()
            .unwrap()
            .split_focused(Direction::Horizontal)
            .unwrap();
        let before = tabs
            .active()
            .unwrap()
            .layout()
            .into_iter()
            .find(|pane| pane.pane == left)
            .unwrap()
            .cols;
        assert!(!close_pane_lifecycle(&mut tabs, right));
        let after = tabs.active().unwrap().layout();
        assert_eq!(after.len(), 1);
        assert!(after[0].cols > before);
    }

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
            Some(PaneAction::Focus(nulix::layout::FocusDirection::Left))
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
            Some(PaneAction::Focus(nulix::layout::FocusDirection::Down))
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
        let first_pty = nulix::pty::Pty::spawn_with_args(
            "/bin/sh",
            ["-c", "printf PANE_ALPHA_MARK; sleep 0.1"],
            &cwd,
            80,
            24,
        )
        .expect("spawn first PTY");
        let second_pty = nulix::pty::Pty::spawn_with_args(
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
