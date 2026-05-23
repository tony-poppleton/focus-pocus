// Transparent bouncing magnifier ball overlay for Windows.
//
// Architecture:
//   * A borderless, always-on-top, fullscreen `egui` window is created over
//     the whole desktop.
//   * Every frame we call SetWindowRgn(HWND, CreateEllipticRgn(...)) so the
//     window region is exactly a circle around the current ball position.
//     Pixels outside that circle are simply not part of the window: the OS
//     composites the real desktop there, and mouse input outside the circle
//     hit-tests to the windows behind us. This is what gives us a
//     transparent overlay *and* a fully interactive desktop behind
//     without depending on LWA_COLORKEY / WS_EX_LAYERED (which don't work
//     reliably with eframe/glow's DirectComposition-backed swap chain --
//     the rendered pixels never reach the GDI redirection bitmap that the
//     colour key operates on, leaving the overlay opaque-black).
//   * SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE) is set so our own
//     window is invisible to GDI screen capture, letting us BitBlt the
//     area beneath the ball without recursively capturing ourselves.
//   * Each frame we BitBlt the area under the ball from the desktop DC,
//     upload it as an `egui` texture, then draw it onto a tessellated
//     circular mesh whose UVs implement a spherical-lens remap (strong
//     magnification at the centre, falling off toward the rim).
//   * Mouse hit-testing for the "click the ball to quit" behaviour is
//     done with GetCursorPos + GetAsyncKeyState so it works regardless
//     of focus and message-routing details.

use eframe::egui;
use std::time::Instant;

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{HWND, POINT},
    Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateEllipticRgn, DeleteDC,
        DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject, SetWindowRgn, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
    },
    UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON},
    UI::WindowsAndMessaging::{
        FindWindowW, GetCursorPos, GetSystemMetrics, GetWindowLongPtrW, SetWindowDisplayAffinity,
        SetWindowLongPtrW, GWL_EXSTYLE, SM_CXSCREEN, SM_CYSCREEN, WDA_EXCLUDEFROMCAPTURE,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    },
};

const WINDOW_TITLE: &str = "EguiMagnifierBall_Overlay";
const BALL_RADIUS: f32 = 180.0;
/// Magnification at the very centre of the ball (the rate at which apparent
/// size grows with object size for the point directly under the ball axis).
const CENTER_MAGNIFICATION: f32 = 6.0;
/// Higher = more spherical curvature (more compression of the image toward
/// the rim). 1.0 would be perfectly flat (uniform zoom); 3..5 looks like a
/// glass marble.
const SPHERE_POWER: f32 = 4.0;
const TIMEOUT_SECS: u64 = 0;
/// Lens tessellation: concentric rings (radial steps) and segments
/// (angular steps). Many radial steps are needed so the spherical UV
/// remap is sampled smoothly along the radius.
const RINGS: usize = 28;
const SEGMENTS: usize = 96;

fn main() -> Result<(), eframe::Error> {
    println!(
        "[magnifier] starting (timeout = {}s, radius = {}, centre zoom = {}x, sphere power = {})",
        TIMEOUT_SECS, BALL_RADIUS, CENTER_MAGNIFICATION, SPHERE_POWER
    );

    #[cfg(windows)]
    let (screen_w, screen_h) = unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN) as f32,
            GetSystemMetrics(SM_CYSCREEN) as f32,
        )
    };
    #[cfg(not(windows))]
    let (screen_w, screen_h) = (1920.0_f32, 1080.0_f32);

    println!("[magnifier] primary screen = {}x{}", screen_w, screen_h);

    // No `with_transparent(true)`: we don't rely on framebuffer-alpha
    // transparency at all. Instead, SetWindowRgn carves the window down to
    // just the circular ball area each frame -- everything outside that
    // circle is genuinely not part of the window, so the desktop shows
    // through and mouse input passes through automatically.
    let viewport = egui::ViewportBuilder::default()
        .with_title(WINDOW_TITLE)
        .with_decorations(false)
        .with_transparent(false)
        .with_always_on_top()
        .with_resizable(false)
        .with_inner_size([screen_w, screen_h])
        .with_position([0.0, 0.0])
        .with_active(false);

    let options = eframe::NativeOptions {
        viewport,
        vsync: true,
        ..Default::default()
    };

    let start = Instant::now();
    let result = eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(MagnifierApp::new(cc, start)))),
    );
    println!("[magnifier] exited");
    result
}

struct MagnifierApp {
    pos: egui::Pos2,
    vel: egui::Vec2,
    last_update: Instant,
    start: Instant,
    texture: Option<egui::TextureHandle>,
    hwnd: isize,
    styles_set: bool,
    excluded_from_capture: bool,
    region_initialized: bool,
    prev_mouse_down: bool,
    closing: bool,
}

impl MagnifierApp {
    fn new(_cc: &eframe::CreationContext<'_>, start: Instant) -> Self {
        Self {
            pos: egui::pos2(320.0, 240.0),
            vel: egui::vec2(230.0, 175.0),
            last_update: Instant::now(),
            start,
            texture: None,
            hwnd: 0,
            styles_set: false,
            excluded_from_capture: false,
            region_initialized: false,
            prev_mouse_down: false,
            closing: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn find_hwnd(title: &str) -> isize {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) as isize }
}

#[cfg(windows)]
fn set_overlay_styles(hwnd_raw: isize) -> bool {
    if hwnd_raw == 0 {
        return false;
    }
    unsafe {
        let hwnd = hwnd_raw as HWND;
        let cur_ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        // WS_EX_TOOLWINDOW    -> no taskbar entry, no alt-tab presence
        // WS_EX_NOACTIVATE    -> clicks on the ball don't steal focus from
        //                        whatever the user was using
        let extra = (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, cur_ex | extra);
    }
    println!("[magnifier] overlay styles applied to hwnd={:#x}", hwnd_raw);
    true
}

/// Set the window's visible/hit-testable region to a circle of `radius_px`
/// pixels centred at (`cx_px`, `cy_px`) in client-area pixels. Everything
/// outside that disc behaves as if the window were not there: the desktop
/// is visible there, and mouse input falls through to the windows behind.
///
/// SetWindowRgn takes ownership of the HRGN, so we must NOT delete it
/// ourselves after a successful call.
#[cfg(windows)]
fn set_window_circle_region(hwnd_raw: isize, cx_px: i32, cy_px: i32, radius_px: i32) {
    if hwnd_raw == 0 {
        return;
    }
    unsafe {
        let rgn = CreateEllipticRgn(
            cx_px - radius_px,
            cy_px - radius_px,
            cx_px + radius_px,
            cy_px + radius_px,
        );
        if rgn.is_null() {
            return;
        }
        // bRedraw = FALSE: egui already redraws every frame, so no need to
        // schedule an extra WM_PAINT (which would cost us a bit of CPU and
        // can cause visible flicker at the rim).
        SetWindowRgn(hwnd_raw as HWND, rgn, 0);
    }
}

/// Hide the overlay from GDI screen capture, so BitBlt'ing the desktop
/// won't include our own ball pixels (which would otherwise produce a
/// recursive smearing artifact). Returns true on success. May fail the
/// first time before the swap chain has presented, so we retry.
#[cfg(windows)]
fn try_exclude_from_capture(hwnd_raw: isize) -> bool {
    if hwnd_raw == 0 {
        return false;
    }
    unsafe { SetWindowDisplayAffinity(hwnd_raw as HWND, WDA_EXCLUDEFROMCAPTURE) != 0 }
}

/// Capture a rectangular region of the primary display.
/// Coordinates and size are in *physical* pixels. Returns RGBA bytes.
#[cfg(windows)]
fn capture_region(x: i32, y: i32, w: i32, h: i32) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        if screen_dc.is_null() {
            return None;
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return None;
        }
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        if bmp.is_null() {
            DeleteDC(mem_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return None;
        }
        let old_obj = SelectObject(mem_dc, bmp as _);

        let blt_ok = BitBlt(mem_dc, 0, 0, w, h, screen_dc, x, y, SRCCOPY);

        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h; // top-down DIB
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB as u32;

        let lines = GetDIBits(
            mem_dc,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut _,
            &mut bi,
            DIB_RGB_COLORS,
        );

        SelectObject(mem_dc, old_obj);
        DeleteObject(bmp as _);
        DeleteDC(mem_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);

        if blt_ok == 0 || lines == 0 {
            return None;
        }

        // Windows DIB returns BGRA; convert in-place to RGBA and force
        // alpha to fully opaque (the captured desktop has no real alpha
        // channel and the bytes there can be garbage).
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }
        Some(buf)
    }
}

#[cfg(windows)]
fn cursor_pos_physical() -> Option<(i32, i32)> {
    unsafe {
        let mut p: POINT = std::mem::zeroed();
        if GetCursorPos(&mut p) != 0 {
            Some((p.x, p.y))
        } else {
            None
        }
    }
}

#[cfg(windows)]
fn left_mouse_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0 }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

impl eframe::App for MagnifierApp {
    /// The clear colour is invisible -- only pixels inside the circular
    /// window region we install each frame are ever shown on screen, and
    /// those are entirely covered by the lens mesh. We still pick opaque
    /// black so that any one-frame races between SetWindowRgn and the
    /// next swap don't briefly flash bright content.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ---- win32 setup (some pieces retry across frames) ---------------
        #[cfg(windows)]
        {
            if self.hwnd == 0 {
                self.hwnd = find_hwnd(WINDOW_TITLE);
            }
            if self.hwnd != 0 && !self.styles_set {
                if set_overlay_styles(self.hwnd) {
                    self.styles_set = true;
                }
            }
            if self.hwnd != 0 && !self.excluded_from_capture {
                if try_exclude_from_capture(self.hwnd) {
                    self.excluded_from_capture = true;
                    println!("[magnifier] WDA_EXCLUDEFROMCAPTURE established");
                }
            }
            // Install the initial circular region as early as possible so the
            // overlay never appears as a full-screen opaque rectangle even for
            // a single frame.
            if self.hwnd != 0 && !self.region_initialized {
                let ppp = ctx.pixels_per_point();
                let r = (BALL_RADIUS * ppp).round() as i32;
                let cx = (self.pos.x * ppp).round() as i32;
                let cy = (self.pos.y * ppp).round() as i32;
                set_window_circle_region(self.hwnd, cx, cy, r);
                self.region_initialized = true;
                println!("[magnifier] initial window region installed");
            }
        }

        // ---- timeout -----------------------------------------------------
        let elapsed = self.start.elapsed();
        // Timeout disabled -- run until clicked.
        // if !self.closing && elapsed.as_secs() >= TIMEOUT_SECS {
        //     println!(
        //         "[magnifier] {}s timeout reached, closing",
        //         TIMEOUT_SECS
        //     );
        //     self.closing = true;
        //     ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        // }

        // ---- physics -----------------------------------------------------
        let now = Instant::now();
        let dt = (now - self.last_update).as_secs_f32().clamp(0.0, 0.05);
        self.last_update = now;

        let screen = ctx.screen_rect();
        let w = screen.width();
        let h = screen.height();

        self.pos += self.vel * dt;
        if self.pos.x - BALL_RADIUS < 0.0 {
            self.pos.x = BALL_RADIUS;
            self.vel.x = self.vel.x.abs();
        }
        if self.pos.x + BALL_RADIUS > w {
            self.pos.x = w - BALL_RADIUS;
            self.vel.x = -self.vel.x.abs();
        }
        if self.pos.y - BALL_RADIUS < 0.0 {
            self.pos.y = BALL_RADIUS;
            self.vel.y = self.vel.y.abs();
        }
        if self.pos.y + BALL_RADIUS > h {
            self.pos.y = h - BALL_RADIUS;
            self.vel.y = -self.vel.y.abs();
        }

        let ppp = ctx.pixels_per_point();

        // ---- click-on-ball detection -------------------------------------
        // The window is click-through, so we sample the global cursor and
        // mouse-button state directly via Win32 and do hit-testing here.
        #[cfg(windows)]
        {
            if let Some((cx_phys, cy_phys)) = cursor_pos_physical() {
                let cx = cx_phys as f32 / ppp;
                let cy = cy_phys as f32 / ppp;
                let dx = cx - self.pos.x;
                let dy = cy - self.pos.y;
                let inside = dx * dx + dy * dy <= BALL_RADIUS * BALL_RADIUS;

                let down = left_mouse_down();
                if !self.closing && down && !self.prev_mouse_down && inside {
                    println!(
                        "[magnifier] click on ball at ({:.0},{:.0}) after {:.2}s, closing",
                        cx,
                        cy,
                        elapsed.as_secs_f32()
                    );
                    self.closing = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                self.prev_mouse_down = down;
            }
        }

        // ---- update the window region to follow the ball ----------------
        // This is what makes the rest of the screen genuinely transparent:
        // outside this circle the window isn't there.
        #[cfg(windows)]
        if self.hwnd != 0 {
            let r = (BALL_RADIUS * ppp).round() as i32;
            let cx = (self.pos.x * ppp).round() as i32;
            let cy = (self.pos.y * ppp).round() as i32;
            set_window_circle_region(self.hwnd, cx, cy, r);
        }

        // ---- capture the screen region under the ball --------------------
        //
        // We capture a square whose *inscribed circle* has radius =
        // BALL_RADIUS, i.e. the area directly under the ball on the
        // desktop. The spherical UV remap below then samples this texture
        // with a strong center bias.
        #[cfg(windows)]
        {
            let region_radius_logical = BALL_RADIUS;
            let cap_x = ((self.pos.x - region_radius_logical) * ppp).round() as i32;
            let cap_y = ((self.pos.y - region_radius_logical) * ppp).round() as i32;
            let cap_size = ((2.0 * region_radius_logical) * ppp).round() as i32;

            if let Some(pixels) = capture_region(cap_x, cap_y, cap_size, cap_size) {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [cap_size as usize, cap_size as usize],
                    &pixels,
                );
                match &mut self.texture {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.texture = Some(ctx.load_texture(
                            "magnifier-lens",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                        println!("[magnifier] lens texture created ({}px)", cap_size);
                    }
                }
            }
        }

        // ---- draw --------------------------------------------------------
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::TRANSPARENT))
            .show(ctx, |ui| {
                let painter = ui.painter();

                if let Some(tex) = &self.texture {
                    painter.add(build_lens_mesh(tex.id(), self.pos));
                }
            });

        // Drive the animation regardless of input events.
        ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------
// Lens mesh
// ---------------------------------------------------------------------------

/// Build a tessellated disc whose UV mapping simulates a glass-sphere
/// magnifier.
///
/// The disc is RINGS concentric rings of SEGMENTS vertices each (plus a
/// single centre vertex). For a vertex at normalized radius `r \u{2208} [0, 1]`
/// on the ball, the texture sample radius is
///
///   f(r) = c \u{00b7} r + (1 \u{2212} c) \u{00b7} r^p           where c = 1 / CENTER_MAGNIFICATION
///                                       p = SPHERE_POWER
///
/// Properties:
///   * f(0) = 0  \u{2192} centre of ball samples centre of capture
///   * f(1) = 1  \u{2192} edge of ball samples edge of capture (no seam)
///   * f'(0) = c \u{2192} centre magnification = 1/c = CENTER_MAGNIFICATION
///   * For p > 1, f grows slowly near r=0 and quickly near r=1 \u{2192} the
///     image is strongly enlarged in the middle and compressed toward the
///     rim, exactly like the view through a glass marble.
fn build_lens_mesh(tex: egui::TextureId, center: egui::Pos2) -> egui::Shape {
    use egui::epaint::Vertex;
    use egui::{pos2, Color32};

    let mut mesh = egui::Mesh::with_texture(tex);
    let c_factor = 1.0 / CENTER_MAGNIFICATION;
    let tau = std::f32::consts::TAU;

    // Vertex 0: centre.
    mesh.vertices.push(Vertex {
        pos: center,
        uv: pos2(0.5, 0.5),
        color: Color32::WHITE,
    });

    // Ring vertices. Ring `k` (1..=RINGS) has SEGMENTS vertices.
    for ring in 1..=RINGS {
        let r_norm = ring as f32 / RINGS as f32;
        let r_pixels = r_norm * BALL_RADIUS;

        // Spherical refraction approximation.
        let f = c_factor * r_norm + (1.0 - c_factor) * r_norm.powf(SPHERE_POWER);
        // UV is in [0,1] across the captured square; centre is (0.5, 0.5)
        // and the inscribed circle of radius 0.5 spans the ball area.
        let uv_off = f * 0.5;

        for seg in 0..SEGMENTS {
            let theta = seg as f32 / SEGMENTS as f32 * tau;
            let (sn, cs) = theta.sin_cos();
            mesh.vertices.push(Vertex {
                pos: pos2(center.x + cs * r_pixels, center.y + sn * r_pixels),
                uv: pos2(0.5 + cs * uv_off, 0.5 + sn * uv_off),
                color: Color32::WHITE,
            });
        }
    }

    // Triangles for ring 1 with the centre vertex (fan).
    for seg in 0..SEGMENTS {
        let v_curr = 1 + seg;
        let v_next = 1 + (seg + 1) % SEGMENTS;
        mesh.indices.push(0);
        mesh.indices.push(v_curr as u32);
        mesh.indices.push(v_next as u32);
    }
    // Triangles between consecutive rings (quad strips).
    for ring in 1..RINGS {
        let base_inner = 1 + (ring - 1) * SEGMENTS;
        let base_outer = 1 + ring * SEGMENTS;
        for seg in 0..SEGMENTS {
            let next_seg = (seg + 1) % SEGMENTS;
            let i_curr = (base_inner + seg) as u32;
            let i_next = (base_inner + next_seg) as u32;
            let o_curr = (base_outer + seg) as u32;
            let o_next = (base_outer + next_seg) as u32;
            mesh.indices.push(i_curr);
            mesh.indices.push(o_curr);
            mesh.indices.push(o_next);
            mesh.indices.push(i_curr);
            mesh.indices.push(o_next);
            mesh.indices.push(i_next);
        }
    }

    egui::Shape::mesh(mesh)
}
