mod app;
mod cell;

use std::{
    ffi::CString,
    num::NonZeroU32,
    time::{Duration, Instant},
};

use gl::types::*;
use glutin::{
    config::{ConfigTemplateBuilder, GlConfig},
    context::{ContextApi, ContextAttributesBuilder, PossiblyCurrentContext},
    display::{GetGlDisplay, GlDisplay},
    prelude::{GlSurface, NotCurrentGlContext},
    surface::{Surface as GlutinSurface, SurfaceAttributesBuilder, WindowSurface},
};
use glutin_winit::DisplayBuilder;
#[allow(deprecated)]
use raw_window_handle::HasRawWindowHandle;
use skia_safe::{
    ColorType, Surface,
    gpu::{self, SurfaceOrigin, backend_render_targets, gl::FramebufferInfo},
};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, Modifiers, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowAttributes, WindowId},
};

use crate::app::KeptApp;

struct Env {
    surface: Surface,
    gl_surface: GlutinSurface<WindowSurface>,
    gr_context: skia_safe::gpu::DirectContext,
    gl_context: PossiblyCurrentContext,
    window: Window,
}

struct Application {
    env: Env,
    fb_info: FramebufferInfo,
    num_samples: usize,
    stencil_size: usize,
    previous_frame_start: Instant,
    dpi: f32,
    modifiers: Modifiers,
    last_mouse_pos: (f32, f32),
    kept_app: KeptApp,
}

fn create_surface(
    window: &Window,
    fb_info: FramebufferInfo,
    gr_context: &mut skia_safe::gpu::DirectContext,
    num_samples: usize,
    stencil_size: usize,
) -> Surface {
    let size = window.inner_size();
    let size = (
        size.width.try_into().expect("width fits i32"),
        size.height.try_into().expect("height fits i32"),
    );
    let backend_render_target =
        backend_render_targets::make_gl(size, num_samples, stencil_size, fb_info);

    gpu::surfaces::wrap_backend_render_target(
        gr_context,
        &backend_render_target,
        SurfaceOrigin::BottomLeft,
        ColorType::RGBA8888,
        None,
        None,
    )
    .expect("could not create skia surface")
}

impl ApplicationHandler for Application {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        match cause {
            winit::event::StartCause::ResumeTimeReached { .. }
            | winit::event::StartCause::Init => {
                self.env.window.request_redraw();
            }
            _ => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let frame_start = Instant::now();

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            WindowEvent::ModifiersChanged(new_modifiers) => {
                self.modifiers = new_modifiers;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if self.kept_app.handle_key(&event, &self.modifiers) {
                    self.env.window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let x = position.x as f32 / self.dpi;
                let y = position.y as f32 / self.dpi;
                self.last_mouse_pos = (x, y);
                if self.kept_app.mouse_drag_to(x, y) {
                    self.env.window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    let (x, y) = self.last_mouse_pos;
                    let changed = match state {
                        ElementState::Pressed => self.kept_app.mouse_down(x, y, &self.modifiers),
                        ElementState::Released => self.kept_app.mouse_up(),
                    };
                    if changed {
                        self.env.window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 30.0,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / self.dpi,
                };
                if self.kept_app.scroll_by(-dy) {
                    self.env.window.request_redraw();
                }
            }
            WindowEvent::Resized(physical_size) => {
                self.env.surface = create_surface(
                    &self.env.window,
                    self.fb_info,
                    &mut self.env.gr_context,
                    self.num_samples,
                    self.stencil_size,
                );
                let (width, height): (u32, u32) = physical_size.into();
                self.env.gl_surface.resize(
                    &self.env.gl_context,
                    NonZeroU32::new(width.max(1)).unwrap(),
                    NonZeroU32::new(height.max(1)).unwrap(),
                );
            }
            WindowEvent::RedrawRequested => {
                let canvas = self.env.surface.canvas();
                let physical = self.env.window.inner_size();
                let logical_w = physical.width as f32 / self.dpi;
                let logical_h = physical.height as f32 / self.dpi;

                canvas.save();
                canvas.scale((self.dpi, self.dpi));
                self.kept_app.tick(canvas, logical_w, logical_h);
                canvas.restore();

                self.env.gr_context.flush_and_submit();
                self.env
                    .gl_surface
                    .swap_buffers(&self.env.gl_context)
                    .unwrap();
            }
            _ => {}
        }

        let frame_duration = Duration::from_secs_f32(1.0 / 60.0);
        if frame_start - self.previous_frame_start > frame_duration {
            self.previous_frame_start = frame_start;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            self.previous_frame_start + frame_duration,
        ));
    }
}

fn main() {
    let el = EventLoop::new().expect("failed to create event loop");

    let window_attributes = WindowAttributes::default()
        .with_title("Kept")
        .with_inner_size(LogicalSize::new(720, 480));

    let template = ConfigTemplateBuilder::new()
        .with_alpha_size(8)
        .with_transparency(true);

    let display_builder = DisplayBuilder::new().with_window_attributes(window_attributes.into());
    let (window, gl_config) = display_builder
        .build(&el, template, |configs| {
            configs
                .reduce(|accum, config| {
                    let transparency_check = config.supports_transparency().unwrap_or(false)
                        & !accum.supports_transparency().unwrap_or(false);
                    if transparency_check || config.num_samples() < accum.num_samples() {
                        config
                    } else {
                        accum
                    }
                })
                .unwrap()
        })
        .unwrap();
    let window = window.expect("could not create window with OpenGL context");

    let dpi = window.scale_factor() as f32;

    #[allow(deprecated)]
    let raw_window_handle = window
        .raw_window_handle()
        .expect("failed to retrieve RawWindowHandle");

    let context_attributes = ContextAttributesBuilder::new().build(Some(raw_window_handle));
    let fallback_context_attributes = ContextAttributesBuilder::new()
        .with_context_api(ContextApi::Gles(None))
        .build(Some(raw_window_handle));
    let not_current_gl_context = unsafe {
        gl_config
            .display()
            .create_context(&gl_config, &context_attributes)
            .unwrap_or_else(|_| {
                gl_config
                    .display()
                    .create_context(&gl_config, &fallback_context_attributes)
                    .expect("failed to create context")
            })
    };

    let (width, height): (u32, u32) = window.inner_size().into();
    let attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
        raw_window_handle,
        NonZeroU32::new(width.max(1)).unwrap(),
        NonZeroU32::new(height.max(1)).unwrap(),
    );

    let gl_surface = unsafe {
        gl_config
            .display()
            .create_window_surface(&gl_config, &attrs)
            .expect("could not create gl window surface")
    };

    let gl_context = not_current_gl_context
        .make_current(&gl_surface)
        .expect("could not make GL context current");

    gl::load_with(|s| {
        gl_config
            .display()
            .get_proc_address(CString::new(s).unwrap().as_c_str())
    });
    let interface = skia_safe::gpu::gl::Interface::new_load_with(|name| {
        if name == "eglGetCurrentDisplay" {
            return std::ptr::null();
        }
        gl_config
            .display()
            .get_proc_address(CString::new(name).unwrap().as_c_str())
    })
    .expect("could not create skia GL interface");

    let mut gr_context = skia_safe::gpu::direct_contexts::make_gl(interface, None)
        .expect("could not create direct context");

    let fb_info = {
        let mut fboid: GLint = 0;
        unsafe { gl::GetIntegerv(gl::FRAMEBUFFER_BINDING, &mut fboid) };
        FramebufferInfo {
            fboid: fboid.try_into().unwrap(),
            format: skia_safe::gpu::gl::Format::RGBA8.into(),
            ..Default::default()
        }
    };

    let num_samples = gl_config.num_samples() as usize;
    let stencil_size = gl_config.stencil_size() as usize;

    let surface = create_surface(&window, fb_info, &mut gr_context, num_samples, stencil_size);

    let env = Env {
        surface,
        gl_surface,
        gl_context,
        gr_context,
        window,
    };

    let mut application = Application {
        env,
        fb_info,
        num_samples,
        stencil_size,
        previous_frame_start: Instant::now(),
        dpi,
        modifiers: Modifiers::default(),
        last_mouse_pos: (0.0, 0.0),
        kept_app: KeptApp::new(),
    };

    el.run_app(&mut application).expect("event loop failed");
}
