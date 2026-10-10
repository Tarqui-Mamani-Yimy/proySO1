use std::sync::Arc;

use so1_terminal::clamp_surface_size;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

struct TerminalWindow {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    failed: bool,
}

struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

impl TerminalWindow {
    fn new() -> Self {
        Self {
            window: None,
            gpu: None,
            failed: false,
        }
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let window = Arc::new(
            event_loop
                .create_window(WindowAttributes::default().with_title("so1term"))
                .map_err(|error| format!("could not create so1term window: {error}"))?,
        );
        let size = window.inner_size();
        let (width, height) = clamp_surface_size(size.width, size.height);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| format!("could not create GPU surface: {error}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|error| {
            format!("could not find a GPU adapter compatible with the window surface: {error}")
        })?;
        let adapter_name = adapter.get_info().name;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("so1term device"),
            ..Default::default()
        }))
        .map_err(|error| format!("could not initialize GPU device: {error}"))?;
        let config = surface
            .get_default_config(&adapter, width, height)
            .ok_or_else(|| "GPU adapter does not support the window surface".to_owned())?;
        surface.configure(&device, &config);

        eprintln!("so1term initialized GPU adapter: {adapter_name}");
        self.window = Some(window);
        self.gpu = Some(GpuState {
            surface,
            device,
            queue,
            config,
        });
        Ok(())
    }

    fn draw(&mut self) -> Result<(), String> {
        let Some(gpu) = self.gpu.as_mut() else {
            return Ok(());
        };
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
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
                label: Some("so1term clear encoder"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("so1term background clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.08,
                            g: 0.09,
                            b: 0.12,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        gpu.queue.submit([encoder.finish()]);
        gpu.queue.present(frame);
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) {
        if let Some(gpu) = self.gpu.as_mut() {
            let (width, height) = clamp_surface_size(width, height);
            gpu.config.width = width;
            gpu.config.height = height;
            gpu.surface.configure(&gpu.device, &gpu.config);
        }
    }
}

impl ApplicationHandler for TerminalWindow {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.failed {
            return;
        }
        if let Err(error) = self.initialize(event_loop) {
            eprintln!("so1term: {error}");
            self.failed = true;
            event_loop.exit();
            return;
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.resize(size.width, size.height);
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => match self.draw() {
                Ok(()) => {
                    // The MVP has no animated content; resize events request new frames.
                }
                Err(error) => {
                    eprintln!("so1term: {error}");
                    self.failed = true;
                    event_loop.exit();
                }
            },
            _ => {}
        }
    }
}

fn main() {
    let event_loop = match EventLoop::new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("so1term: could not initialize the window event loop: {error}");
            std::process::exit(1);
        }
    };
    let mut app = TerminalWindow::new();
    if let Err(error) = event_loop.run_app(&mut app) {
        eprintln!("so1term: window event loop failed: {error}");
        std::process::exit(1);
    }
    if app.failed {
        std::process::exit(1);
    }
}
