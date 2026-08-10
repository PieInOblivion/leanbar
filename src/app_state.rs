use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use std::os::fd::AsFd;
use std::ptr;

use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_surface::WlSurface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use crate::bar_renderer::BarRenderer;
use crate::{error::LeanbarError, font::FontAtlas};

const BAR_HEIGHT: usize = 28;

#[derive(Default)]
pub struct WaylandSurface {
    pub layer_surface: Option<ZwlrLayerSurfaceV1>,
    pub wl_surface: Option<WlSurface>,
    pub buffer: Option<WlBuffer>,
    pub pixels: Option<ShmBuffer>,
    pub width: usize,
    pub height: usize,
    pub configured: bool,
}

pub struct AppState {
    pub compositor: Option<WlCompositor>,
    pub shm: Option<WlShm>,
    pub layer_shell: Option<ZwlrLayerShellV1>,

    pub surface: WaylandSurface,
    pub renderer: BarRenderer,
    pub force_full_redraw: bool,
}

impl AppState {
    pub fn new(atlas: FontAtlas) -> Self {
        Self {
            compositor: None,
            shm: None,
            layer_shell: None,
            surface: WaylandSurface::default(),
            renderer: BarRenderer::new(atlas),
            force_full_redraw: true,
        }
    }

    pub fn has_required_globals(&self) -> bool {
        self.compositor.is_some() && self.shm.is_some() && self.layer_shell.is_some()
    }

    pub fn initialize_layer_surface(&mut self, qh: &QueueHandle<Self>) -> Result<(), LeanbarError> {
        let compositor = self
            .compositor
            .as_ref()
            .ok_or_else(|| LeanbarError::Wayland("missing wl_compositor".into()))?;
        let layer_shell = self
            .layer_shell
            .as_ref()
            .ok_or_else(|| LeanbarError::Wayland("missing zwlr_layer_shell_v1".into()))?;

        let wl_surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &wl_surface,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "leanbar".to_string(),
            qh,
            (),
        );

        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer_surface.set_size(0, BAR_HEIGHT as u32);
        layer_surface.set_exclusive_zone(BAR_HEIGHT as i32);

        wl_surface.commit();

        self.surface.wl_surface = Some(wl_surface);
        self.surface.layer_surface = Some(layer_surface);

        Ok(())
    }

    pub fn redraw_and_commit(&mut self) {
        if !self.surface.configured {
            return;
        }

        let force = self.force_full_redraw;
        let rendered = if let (Some(surface), Some(pixels_buf)) =
            (&self.surface.wl_surface, &mut self.surface.pixels)
        {
            let pixels = pixels_buf.as_slice_mut();

            self.renderer.render_frame(
                pixels,
                self.surface.width,
                self.surface.height,
                force,
                |x, w| {
                    surface.damage_buffer(x as i32, 0, w as i32, self.surface.height as i32);
                },
            )
        } else {
            false
        };

        if rendered {
            self.force_full_redraw = false;
            if let (Some(surface), Some(buffer)) = (&self.surface.wl_surface, &self.surface.buffer)
            {
                surface.attach(Some(buffer), 0, 0);
                surface.commit();
            }
        }
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        if let Some(buffer) = self.surface.buffer.take() {
            buffer.destroy();
        }
    }
}

/// Safe wrapper around shared memory map for Wayland pixel buffer
pub struct ShmBuffer {
    ptr: *mut u32,
    size: usize,
}

impl ShmBuffer {
    pub fn new(memfd: &rustix::fd::OwnedFd, size: usize) -> Result<Self, LeanbarError> {
        let ptr = unsafe {
            mmap(
                ptr::null_mut(),
                size,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                memfd,
                0,
            )?
        };
        Ok(Self {
            ptr: ptr.cast(),
            size,
        })
    }

    pub fn as_slice_mut(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size / 4) }
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.size > 0 {
            let _ = unsafe { munmap(self.ptr.cast(), self.size) };
        }
    }
}

impl Dispatch<WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, 4, qhandle, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, 1, qhandle, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, 4, qhandle, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            layer_surface.ack_configure(serial);

            let w = if width == 0 { 1920 } else { width };
            let h = if height == 0 {
                BAR_HEIGHT as u32
            } else {
                height
            };

            if state.surface.width != w as usize || state.surface.height != h as usize {
                if let Some(old_buffer) = state.surface.buffer.take() {
                    old_buffer.destroy();
                }

                state.surface.pixels = None;

                state.surface.width = w as usize;
                state.surface.height = h as usize;

                let stride = w * 4;
                let size = (stride * h) as usize;

                let memfd = memfd_create("leanbar-shm", MemfdFlags::CLOEXEC).unwrap();
                ftruncate(&memfd, size as u64).unwrap();

                state.surface.pixels = Some(ShmBuffer::new(&memfd, size).unwrap());

                let pool = state
                    .shm
                    .as_ref()
                    .expect("wl_shm must exist after globals discovery")
                    .create_pool(memfd.as_fd(), size as i32, qhandle, ());
                let buffer = pool.create_buffer(
                    0,
                    w as i32,
                    h as i32,
                    stride as i32,
                    wl_shm::Format::Argb8888,
                    qhandle,
                    (),
                );
                state.surface.buffer = Some(buffer);
            }

            state.surface.configured = true;
            state.force_full_redraw = true;
            state.redraw_and_commit();
        }
    }
}

wayland_client::delegate_noop!(AppState: ignore WlCompositor);
wayland_client::delegate_noop!(AppState: ignore WlShm);
wayland_client::delegate_noop!(AppState: ignore ZwlrLayerShellV1);
wayland_client::delegate_noop!(AppState: ignore WlSurface);
wayland_client::delegate_noop!(AppState: ignore WlBuffer);
wayland_client::delegate_noop!(AppState: ignore wayland_client::protocol::wl_shm_pool::WlShmPool);
