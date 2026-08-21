//! GPU background rendering via EGL/GLES2, with the chrome composited in
//! software (tiny-skia) on top — the hybrid that makes the Ken Burns pan
//! smooth without a GPU-hungry full renderer.
//!
//! The lock surface's `wl_surface` is wrapped in a `wl_egl_window`; each
//! frame the wallpaper is drawn as a full-screen textured quad whose shader
//! applies the pan transform (GPU bilinear filtering makes sub-pixel motion
//! free — the ~19 ms/frame software bilinear is gone) and the vertical dim
//! veil. The chrome (clock/date/pill/status) is still composed by
//! `render::compose_chrome` into a transparent pixmap and blitted to a
//! texture each frame (only the bounding rect of what was drawn).
//!
//! If EGL initialization fails for any reason (headless, no GPU, compositor
//! without EGL), [`GpuRenderer::new`] returns `None` and the locker falls
//! back to the fully-software path unchanged.

use crate::render::{self, FrameInputs};
use breadlock_ui::config::{Background as BackgroundConfig, BackgroundMode};
use breadlock_ui::painter::TextRenderer;
use breadlock_ui::theme::Palette;
use glow::HasContext;
use khronos_egl as egl;
use std::os::raw::c_void;
use tiny_skia::Pixmap;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};

// Same pan geometry as `background.rs` — kept in sync by comment.
const KENBURNS_PERIOD_S: f32 = 90.0;
const KENBURNS_ZOOM: f32 = 1.06;

const EGL_ATTRIBS: [egl::Int; 11] = [
    egl::SURFACE_TYPE,
    (egl::WINDOW_BIT | egl::PBUFFER_BIT) as egl::Int,
    egl::RED_SIZE,
    8,
    egl::GREEN_SIZE,
    8,
    egl::BLUE_SIZE,
    8,
    egl::ALPHA_SIZE,
    8,
    egl::NONE,
];

const VERTEX_SRC: &str = "\
attribute vec2 a_pos;             // pixels, (0,0) top-left
uniform vec2 u_screen;
uniform vec2 u_uv_scale;
uniform vec2 u_uv_offset;
varying vec2 v_uv;
void main() {
    v_uv = a_pos * u_uv_scale + u_uv_offset;
    vec2 clip = vec2(a_pos.x / u_screen.x * 2.0 - 1.0, 1.0 - a_pos.y / u_screen.y * 2.0);
    gl_Position = vec4(clip, 0.0, 1.0);
}";

// Background: sample the wallpaper (or a 1x1 white texture for solid color),
// apply the vertical dim veil. v_uv has v = 0 at the top of the image.
const BG_FRAG_SRC: &str = "\
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex;
uniform vec4 u_color;
uniform float u_dim_top;
uniform float u_dim_bottom;
uniform float u_veil_alpha;
uniform float u_screen_h;
void main() {
    vec4 c = texture2D(u_tex, v_uv) * u_color;
    float row = 1.0 - gl_FragCoord.y / u_screen_h;   // 1 at top
    float dim = mix(u_dim_top, u_dim_bottom, row) * u_veil_alpha;
    gl_FragColor = vec4(c.rgb * (1.0 - dim), 1.0);
}";

// Chrome: premultiplied alpha texture, blended with GL_ONE / ONE_MINUS_SRC_ALPHA.
const CHROME_FRAG_SRC: &str = "\
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_tex;
void main() {
    gl_FragColor = texture2D(u_tex, v_uv);
}";

// The wl_egl_window C API (libwayland-egl). The window wraps a wl_surface
// so EGL can allocate its buffers against the lock surface.
#[repr(C)]
struct wl_surface {
    _private: [u8; 0],
}
#[repr(C)]
pub struct wl_egl_window {
    _private: [u8; 0],
}

#[link(name = "wayland-egl")]
extern "C" {
    fn wl_egl_window_create(surface: *mut wl_surface, width: i32, height: i32) -> *mut wl_egl_window;
    fn wl_egl_window_resize(window: *mut wl_egl_window, width: i32, height: i32, dx: i32, dy: i32);
}

// EGL objects are intentionally not destroyed on the way out: the process
// exits immediately after unlock, and dropping the pbuffer/context while it
// might still be current would be UB — leaving them for the OS is cleaner.
const _: () = ();

/// One EGL-backed lock surface. Created lazily on the first `configure` (the
/// size is unknown before that) and resized on subsequent ones. The process
/// exits right after unlock, so EGL objects are deliberately not destroyed
/// individually.
pub struct GpuSurface {
    egl_window: *mut wl_egl_window,
    egl_surface: egl::Surface,
    width: u32,
    height: u32,
}

impl GpuSurface {
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        // SAFETY: `egl_window` is the pointer `create_surface` stored.
        unsafe { wl_egl_window_resize(self.egl_window, width as i32, height as i32, 0, 0) };
        self.width = width;
        self.height = height;
    }
}

struct Wallpaper {
    tex: glow::Texture,
    size: (u32, u32),
    ken_burns: bool,
}

pub struct GpuRenderer {
    egl: egl::DynamicInstance<egl::EGL1_4>,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    /// 1x1 pbuffer used to make the context current during setup (before any
    /// real lock surface exists). Kept alive for the renderer's lifetime —
    /// the read is deliberate: dropping it while the context might still be
    /// current on it is undefined behavior.
    #[allow(dead_code)]
    setup_surface: egl::Surface,
    gl: glow::Context,
    bg_program: glow::Program,
    chrome_program: glow::Program,
    quad_vao: glow::VertexArray,
    quad_vbo: glow::Buffer,
    wallpaper: Option<Wallpaper>,
    /// 1x1 white texture for solid-color backgrounds (shader multiplies by
    /// the palette color).
    white_tex: glow::Texture,
    bg_color: [f32; 4],
    chrome_tex: glow::Texture,
    chrome_tex_size: (u32, u32),
    /// Reused scratch for the chrome compose.
    chrome_pixmap: Option<Pixmap>,
    u_screen: [Option<glow::UniformLocation>; 2],
    u_uv_scale: [Option<glow::UniformLocation>; 2],
    u_uv_offset: [Option<glow::UniformLocation>; 2],
    u_tex: [Option<glow::UniformLocation>; 2],
    u_color: Option<glow::UniformLocation>,
    u_dim_top: Option<glow::UniformLocation>,
    u_dim_bottom: Option<glow::UniformLocation>,
    u_veil_alpha: Option<glow::UniformLocation>,
    u_screen_h: Option<glow::UniformLocation>,
}

impl GpuRenderer {
    /// Initializes EGL/GLES2 against the session's Wayland display and loads
    /// the wallpaper into a texture. Returns `None` (after logging) on any
    /// failure — the caller keeps the software path.
    pub fn new(conn: &Connection, bg_cfg: &BackgroundConfig, palette: &Palette) -> Option<Self> {
        // SAFETY: khronos-egl's dynamic instance loads libEGL.so.1; the
        // returned handles are only used while the library stays loaded.
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required() }.ok()?;
        // SAFETY: the display pointer comes from our live wayland connection.
        let display = unsafe { egl.get_display(conn.display().id().as_ptr() as *mut c_void) }?;
        egl.initialize(display).ok()?;
        let mut configs = Vec::with_capacity(1);
        egl.choose_config(display, &EGL_ATTRIBS, &mut configs).ok()?;
        let config = *configs.first()?;
        let context = egl
            .create_context(display, config, None, &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE])
            .ok()?;
        // A 1x1 pbuffer is enough to make the context current for setup
        // before any real lock surface exists (pbuffers size via
        // EGL_WIDTH/EGL_HEIGHT).
        let setup_surface = egl
            .create_pbuffer_surface(display, config, &[egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE])
            .ok()?;
        if egl
            .make_current(display, Some(setup_surface), Some(setup_surface), Some(context))
            .is_err()
        {
            return None;
        }

        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|name| {
                egl.get_proc_address(name.to_str().unwrap_or(""))
                    .map(|p| p as *const c_void)
                    .unwrap_or(std::ptr::null())
            })
        };

        let bg_program = compile_program(&gl, VERTEX_SRC, BG_FRAG_SRC)?;
        let chrome_program = compile_program(&gl, VERTEX_SRC, CHROME_FRAG_SRC)?;

        // Fullscreen quad: two triangles covering [0, w] x [0, h] (pixel
        // space). A single unit quad scaled by `u_screen` in the shader
        // would need a uniform; instead the vertices are normalized and the
        // vertex shader multiplies by u_screen... but a_pos is in pixels —
        // so upload actual pixel positions per surface size? No: keep the
        // quad in unit space and let the shader's u_screen scale it. The
        // shader expects a_pos in pixels, so upload a 1x1 unit quad scaled
        // at bind time via glVertexAttrib? Simpler: use normalized coords.
        let quad_vao = unsafe { gl.create_vertex_array() }.ok()?;
        let quad_vbo = unsafe { gl.create_buffer() }.ok()?;
        unsafe {
            gl.bind_vertex_array(Some(quad_vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(quad_vbo));
            // Unit quad [0,1]^2; the vertex shader multiplies by u_screen.
            let verts: [f32; 12] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0];
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, f32s_as_bytes(&verts), glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        }

        // Wallpaper texture (original resolution; the GPU downscales + mipmaps).
        let wallpaper = match &bg_cfg.mode {
            BackgroundMode::Color => None,
            BackgroundMode::Image if bg_cfg.path.is_empty() => {
                tracing::warn!("background.mode = \"image\" but background.path is empty, using solid color");
                None
            }
            BackgroundMode::Image => match Pixmap::load_png(&bg_cfg.path) {
                Ok(pix) => {
                    let (w, h) = (pix.width(), pix.height());
                    let tex = unsafe { gl.create_texture() }.ok()?;
                    unsafe {
                        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                        gl.tex_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            glow::RGBA as i32,
                            w as i32,
                            h as i32,
                            0,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(pix.data())),
                        );
                        gl.generate_mipmap(glow::TEXTURE_2D);
                        gl.tex_parameter_i32(
                            glow::TEXTURE_2D,
                            glow::TEXTURE_MIN_FILTER,
                            glow::LINEAR_MIPMAP_LINEAR as i32,
                        );
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                    }
                    Some(Wallpaper {
                        tex,
                        size: (w, h),
                        ken_burns: bg_cfg.ken_burns,
                    })
                }
                Err(err) => {
                    tracing::warn!(path = %bg_cfg.path, %err, "GPU: failed to load background image, using solid color");
                    None
                }
            },
        };

        // 1x1 white texture for the solid-color shader path.
        let white_tex = unsafe { gl.create_texture() }.ok()?;
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(white_tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                1,
                1,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&[255, 255, 255, 255])),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
        }

        // Full-size chrome texture (sub-image uploaded per frame).
        let chrome_tex = unsafe { gl.create_texture() }.ok()?;
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(chrome_tex));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        }

        let bg = breadlock_ui::theme::tiny_skia_color(&palette.background);
        let bg_color = [bg.red(), bg.green(), bg.blue(), 1.0];

        // Resolve all uniform locations up front, then drop the closure so
        // `gl` can move into the renderer.
        let (u_screen, u_uv_scale, u_uv_offset, u_tex, u_color, u_dim_top, u_dim_bottom, u_veil_alpha, u_screen_h) = {
            let loc = |p: glow::Program, n: &str| unsafe { gl.get_uniform_location(p, n) };
            (
                [loc(bg_program, "u_screen"), loc(chrome_program, "u_screen")],
                [loc(bg_program, "u_uv_scale"), loc(chrome_program, "u_uv_scale")],
                [loc(bg_program, "u_uv_offset"), loc(chrome_program, "u_uv_offset")],
                [loc(bg_program, "u_tex"), loc(chrome_program, "u_tex")],
                loc(bg_program, "u_color"),
                loc(bg_program, "u_dim_top"),
                loc(bg_program, "u_dim_bottom"),
                loc(bg_program, "u_veil_alpha"),
                loc(bg_program, "u_screen_h"),
            )
        };

        Some(Self {
            egl,
            display,
            config,
            context,
            setup_surface,
            gl,
            bg_program,
            chrome_program,
            quad_vao,
            quad_vbo,
            wallpaper,
            white_tex,
            bg_color,
            chrome_tex,
            chrome_tex_size: (0, 0),
            chrome_pixmap: None,
            u_screen,
            u_uv_scale,
            u_uv_offset,
            u_tex,
            u_color,
            u_dim_top,
            u_dim_bottom,
            u_veil_alpha,
            u_screen_h,
        })
    }

    /// Wraps a lock surface's `wl_surface` in an EGL window + surface.
    /// Called once per surface from its first `configure`.
    pub fn create_surface(&self, surface: &WlSurface, width: u32, height: u32) -> Option<GpuSurface> {
        // SAFETY: the surface proxy is live (this is called from its
        // `configure` handler); the returned window is owned by us.
        let egl_window = unsafe {
            wl_egl_window_create(
                surface.id().as_ptr() as *mut wl_surface,
                width as i32,
                height as i32,
            )
        };
        if egl_window.is_null() {
            tracing::error!("wl_egl_window_create failed");
            return None;
        }
        // SAFETY: `egl_window` is a valid wl_egl_window native window.
        let egl_surface = unsafe {
            self.egl
                .create_window_surface(self.display, self.config, egl_window as *mut c_void, None)
        }
        .ok()?;
        Some(GpuSurface {
            egl_window,
            egl_surface,
            width,
            height,
        })
    }

    /// Renders one frame for `surface`: wallpaper quad (pan + veil in the
    /// shader), then the software-composed chrome blitted over it.
    pub fn render_frame(
        &mut self,
        surface: &mut GpuSurface,
        inputs: &FrameInputs,
        text: &mut TextRenderer,
    ) {
        let (w, h) = (surface.width, surface.height);
        if w == 0 || h == 0 {
            return;
        }
        if self
            .egl
            .make_current(self.display, Some(surface.egl_surface), Some(surface.egl_surface), Some(self.context))
            .is_err()
        {
            return;
        }
        let gl = &self.gl;
        unsafe { gl.viewport(0, 0, w as i32, h as i32) };
        self.draw_background(w, h, inputs);
        self.draw_chrome(w, h, inputs, text);
        let _ = self.egl.swap_buffers(self.display, surface.egl_surface);
    }

    fn draw_background(&mut self, w: u32, h: u32, inputs: &FrameInputs) {
        let gl = &self.gl;
        let (veil_alpha, _) = render::overlay_motion(inputs.appear_t, inputs.unlock_t);
        unsafe {
            gl.use_program(Some(self.bg_program));
            gl.bind_vertex_array(Some(self.quad_vao));
            // Unit quad -> pixels: the vertex shader uses a_pos in pixels, so
            // upload the quad scaled... a_pos IS in pixels only if we pass
            // pixel positions; with a unit quad, scale here instead.
            // The vertex shader treats a_pos as pixels and divides by
            // u_screen — for a unit quad we pass a_pos * screen, so set the
            // buffer? Simpler: keep unit quad and multiply u_screen into the
            // uv math in the shader. To avoid shader churn: upload a full
            // pixel-space quad per surface size.
            let wf = w as f32;
            let hf = h as f32;
            let verts: [f32; 12] = [
                0.0, 0.0, wf, 0.0, 0.0, hf, //
                wf, 0.0, wf, hf, 0.0, hf,
            ];
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.quad_vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, f32s_as_bytes(&verts), glow::DYNAMIC_DRAW);

            if let Some(loc) = self.u_screen[0].as_ref() {
                gl.uniform_2_f32(Some(loc), wf, hf);
            }
            if let Some(loc) = self.u_uv_scale[0].as_ref() {
                match &self.wallpaper {
                    Some(wp) => {
                        let (_, _, scaled_w, scaled_h) =
                            pan_region(wp.size, (w, h), wp.ken_burns, inputs.t_secs);
                        gl.uniform_2_f32(Some(loc), 1.0 / scaled_w, 1.0 / scaled_h);
                    }
                    None => gl.uniform_2_f32(Some(loc), 0.0, 0.0),
                }
            }
            if let Some(loc) = self.u_uv_offset[0].as_ref() {
                match &self.wallpaper {
                    Some(wp) => {
                        let (sx0, sy0, scaled_w, scaled_h) =
                            pan_region(wp.size, (w, h), wp.ken_burns, inputs.t_secs);
                        gl.uniform_2_f32(Some(loc), sx0 / scaled_w, sy0 / scaled_h);
                    }
                    None => gl.uniform_2_f32(Some(loc), 0.0, 0.0),
                }
            }
            if let Some(loc) = self.u_color.as_ref() {
                match &self.wallpaper {
                    Some(_) => gl.uniform_4_f32(Some(loc), 1.0, 1.0, 1.0, 1.0),
                    None => gl.uniform_4_f32(
                        Some(loc),
                        self.bg_color[0],
                        self.bg_color[1],
                        self.bg_color[2],
                        1.0,
                    ),
                }
            }
            if let Some(loc) = self.u_dim_top.as_ref() {
                gl.uniform_1_f32(Some(loc), render::DIM_ALPHA_TOP);
            }
            if let Some(loc) = self.u_dim_bottom.as_ref() {
                gl.uniform_1_f32(Some(loc), render::DIM_ALPHA_BOTTOM);
            }
            if let Some(loc) = self.u_veil_alpha.as_ref() {
                gl.uniform_1_f32(Some(loc), veil_alpha);
            }
            if let Some(loc) = self.u_screen_h.as_ref() {
                gl.uniform_1_f32(Some(loc), h as f32);
            }
            gl.active_texture(glow::TEXTURE0);
            match &self.wallpaper {
                Some(wp) => gl.bind_texture(glow::TEXTURE_2D, Some(wp.tex)),
                None => gl.bind_texture(glow::TEXTURE_2D, Some(self.white_tex)),
            }
            if let Some(loc) = self.u_tex[0].as_ref() {
                gl.uniform_1_i32(Some(loc), 0);
            }
            gl.disable(glow::BLEND);
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
        }
    }

    fn draw_chrome(&mut self, w: u32, h: u32, inputs: &FrameInputs, text: &mut TextRenderer) {
        let dirty = self
            .chrome_pixmap
            .as_ref()
            .map(|p| (p.width(), p.height()) != (w, h))
            .unwrap_or(true);
        if dirty {
            self.chrome_pixmap = Pixmap::new(w, h);
        }
        let Some(pixmap) = self.chrome_pixmap.as_mut() else {
            return;
        };
        let rect = render::compose_chrome(pixmap, text, inputs);
        let x0 = rect.x0.max(0.0).floor() as i32;
        let y0 = rect.y0.max(0.0).floor() as i32;
        let x1 = (rect.x1.min(w as f32)).ceil() as i32;
        let y1 = (rect.y1.min(h as f32)).ceil() as i32;
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        if self.chrome_tex_size != (w, h) {
            let gl = &self.gl;
            unsafe {
                gl.bind_texture(glow::TEXTURE_2D, Some(self.chrome_tex));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA as i32,
                    w as i32,
                    h as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
            }
            self.chrome_tex_size = (w, h);
        }

        let data = pixmap.data();
        let stride = w as usize * 4;
        let offset = y0 as usize * stride + x0 as usize * 4;
        let rw = (x1 - x0) as i32;
        let rh = (y1 - y0) as i32;
        let gl = &self.gl;
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.chrome_tex));
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x0,
                y0,
                rw,
                rh,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&data[offset..])),
            );
            gl.use_program(Some(self.chrome_program));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.quad_vbo));
            let wf = w as f32;
            let hf = h as f32;
            let verts: [f32; 12] = [
                0.0, 0.0, wf, 0.0, 0.0, hf, //
                wf, 0.0, wf, hf, 0.0, hf,
            ];
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, f32s_as_bytes(&verts), glow::DYNAMIC_DRAW);
            if let Some(loc) = self.u_screen[1].as_ref() {
                gl.uniform_2_f32(Some(loc), wf, hf);
            }
            if let Some(loc) = self.u_uv_scale[1].as_ref() {
                gl.uniform_2_f32(Some(loc), 1.0 / wf, 1.0 / hf);
            }
            if let Some(loc) = self.u_uv_offset[1].as_ref() {
                gl.uniform_2_f32(Some(loc), 0.0, 0.0);
            }
            if let Some(loc) = self.u_tex[1].as_ref() {
                gl.uniform_1_i32(Some(loc), 0);
            }
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.chrome_tex));
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
            gl.disable(glow::BLEND);
        }
    }
}

/// Visible source region of the wallpaper for the current pan phase — the
/// same cover-fit + Ken Burns math as `background.rs`.
fn pan_region(wp: (u32, u32), target: (u32, u32), ken_burns: bool, t_secs: f32) -> (f32, f32, f32, f32) {
    let (sw, sh) = (wp.0 as f32, wp.1 as f32);
    let (tw, th) = (target.0 as f32, target.1 as f32);
    let cover = (tw / sw).max(th / sh);
    let scale = cover * if ken_burns { KENBURNS_ZOOM } else { 1.0 };
    let scaled_w = sw * scale;
    let scaled_h = sh * scale;
    let pan_x = (scaled_w - tw).max(0.0);
    let pan_y = (scaled_h - th).max(0.0);
    let (tx, ty) = if ken_burns {
        let phase = t_secs * std::f32::consts::TAU / KENBURNS_PERIOD_S;
        (
            -pan_x * (0.5 + 0.5 * phase.sin()),
            -pan_y * (0.5 + 0.5 * phase.cos()),
        )
    } else {
        (0.0, 0.0)
    };
    (-tx, -ty, scaled_w, scaled_h)
}

fn compile_program(gl: &glow::Context, vs_src: &str, fs_src: &str) -> Option<glow::Program> {
    unsafe {
        let program = gl.create_program().ok()?;
        let vs_sh = gl.create_shader(glow::VERTEX_SHADER).ok()?;
        gl.shader_source(vs_sh, vs_src);
        gl.compile_shader(vs_sh);
        if !gl.get_shader_compile_status(vs_sh) {
            let log = gl.get_shader_info_log(vs_sh);
            tracing::error!(%log, "GPU: vertex shader compile failed");
            return None;
        }
        let fs_sh = gl.create_shader(glow::FRAGMENT_SHADER).ok()?;
        gl.shader_source(fs_sh, fs_src);
        gl.compile_shader(fs_sh);
        if !gl.get_shader_compile_status(fs_sh) {
            let log = gl.get_shader_info_log(fs_sh);
            tracing::error!(%log, "GPU: fragment shader compile failed");
            return None;
        }
        gl.attach_shader(program, vs_sh);
        gl.attach_shader(program, fs_sh);
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            tracing::error!(%log, "GPU: program link failed");
            return None;
        }
        gl.delete_shader(vs_sh);
        gl.delete_shader(fs_sh);
        Some(program)
    }
}

fn f32s_as_bytes(v: &[f32; 12]) -> &[u8] {
    // SAFETY: f32 is POD; the byte length is exact.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// The software path's pan math (background.rs `Background::Image::paint`),
    /// re-implemented here so the GPU `pan_region` can be checked against it.
    /// Software rounds the scaled dims to pixels; GPU keeps floats, so
    /// compare with a 1px tolerance.
    fn software_pan(wp: (u32, u32), target: (u32, u32), ken_burns: bool, t_secs: f32) -> (f32, f32) {
        let (sw, sh) = (wp.0 as f32, wp.1 as f32);
        let (tw, th) = (target.0 as f32, target.1 as f32);
        let cover = (tw / sw).max(th / sh);
        let scale = cover * if ken_burns { KENBURNS_ZOOM } else { 1.0 };
        let scaled_w = (sw * scale).round().max(1.0);
        let scaled_h = (sh * scale).round().max(1.0);
        let pan_x = scaled_w - tw;
        let pan_y = scaled_h - th;
        let (tx, ty) = if ken_burns {
            let phase = t_secs * TAU / KENBURNS_PERIOD_S;
            (
                -pan_x * (0.5 + 0.5 * phase.sin()),
                -pan_y * (0.5 + 0.5 * phase.cos()),
            )
        } else {
            (0.0, 0.0)
        };
        (-tx, -ty)
    }

    #[test]
    fn pan_region_static_matches_software_centered() {
        let wp = (3840, 2160);
        let target = (1920, 1200);
        let (sx, sy, sw, sh) = pan_region(wp, target, false, 123.4);
        assert_eq!(sx, 0.0, "no ken burns: no horizontal pan");
        assert_eq!(sy, 0.0, "no ken burns: no vertical pan");
        // Cover fit: the scaled region covers the target in both axes.
        assert!(sw >= 1920.0 && sh >= 1200.0);
        // And it's the tightest cover: at least one axis exactly matches.
        assert!(
            (sw - 1920.0).abs() < 0.01 || (sh - 1200.0).abs() < 0.01,
            "cover must be tight, got {sw}x{sh}"
        );
    }

    #[test]
    fn pan_region_ken_burns_tracks_software_path() {
        let wp = (3840, 2160);
        let target = (1920, 1200);
        for i in 0..=40 {
            let t = i as f32 / 40.0 * KENBURNS_PERIOD_S;
            let (sx, sy, _, _) = pan_region(wp, target, true, t);
            let (ex, ey) = software_pan(wp, target, true, t);
            assert!(
                (sx - ex).abs() < 1.0,
                "x pan diverged from software at t={t}: gpu {sx} vs sw {ex}"
            );
            assert!(
                (sy - ey).abs() < 1.0,
                "y pan diverged from software at t={t}: gpu {sy} vs sw {ey}"
            );
        }
    }

    #[test]
    fn pan_region_never_exposes_edges() {
        let wp = (3840, 2160);
        let target = (1920, 1200);
        for i in 0..=200 {
            let t = i as f32 / 200.0 * KENBURNS_PERIOD_S;
            let (sx, sy, sw, sh) = pan_region(wp, target, true, t);
            assert!(sx >= -0.001, "negative x offset at t={t}");
            assert!(sy >= -0.001, "negative y offset at t={t}");
            assert!(
                sx + 1920.0 <= sw + 0.001,
                "right edge exposed at t={t}: sx {sx} + 1920 > sw {sw}"
            );
            assert!(
                sy + 1200.0 <= sh + 0.001,
                "bottom edge exposed at t={t}: sy {sy} + 1200 > sh {sh}"
            );
        }
    }

    #[test]
    fn pan_region_starts_at_corner_and_returns() {
        // t=0: sin=0, cos=1 → the region sits at the top, horizontally centered.
        let wp = (3840, 2160);
        let target = (1920, 1200);
        let (sx0, sy0, sw, _) = pan_region(wp, target, true, 0.0);
        let pan_x = sw - 1920.0;
        let cover = (1920.0f32 / 3840.0).max(1200.0f32 / 2160.0);
        let pan_y = (2160.0 * (cover * KENBURNS_ZOOM)).round() - 1200.0;
        assert!((sx0 - pan_x * 0.5).abs() < 0.5, "at t=0 x should be half-panned, got {sx0}");
        assert!((sy0 - pan_y).abs() < 0.5, "at t=0 y should be fully panned (top), got {sy0}");
        // Half a period later it has returned to the same spot.
        let (sx1, sy1, _, _) = pan_region(wp, target, true, KENBURNS_PERIOD_S);
        assert!((sx1 - sx0).abs() < 0.01 && (sy1 - sy0).abs() < 0.01);
    }

    /// Extracts every `uniform <type> <name>;` declaration from a GLSL source.
    fn declared_uniforms(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in src.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("uniform ") {
                if let Some((_, name)) = rest.rsplit_once(' ') {
                    out.push(name.trim_end_matches(';').to_string());
                }
            }
        }
        out
    }

    #[test]
    fn shaders_declare_every_uniform_the_renderer_sets() {
        // If a uniform is renamed in the GLSL but not at the call site (or
        // vice versa) it silently becomes -1 and the frame renders wrong;
        // this test pins the two together.
        let declared = [
            declared_uniforms(VERTEX_SRC),
            declared_uniforms(BG_FRAG_SRC),
            declared_uniforms(CHROME_FRAG_SRC),
        ]
        .concat();
        for name in [
            "u_screen", "u_uv_scale", "u_uv_offset", "u_tex", "u_color",
            "u_dim_top", "u_dim_bottom", "u_veil_alpha", "u_screen_h",
        ] {
            assert!(
                declared.iter().any(|d| d == name),
                "uniform {name} missing from shader sources"
            );
        }
    }

    #[test]
    fn egl_attribs_are_none_terminated_pairs() {
        assert_eq!(EGL_ATTRIBS.len() % 2, 1, "attribs must be key/value pairs + NONE");
        assert_eq!(*EGL_ATTRIBS.last().unwrap(), egl::NONE, "attrib list must be NONE-terminated");
    }

    #[test]
    fn f32s_as_bytes_has_exact_length() {
        let v: [f32; 12] = [0.0; 12];
        assert_eq!(f32s_as_bytes(&v).len(), 12 * 4);
    }
}
