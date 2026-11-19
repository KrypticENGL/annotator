#![windows_subsystem = "windows"]
//! Transparent, click-through desktop annotation overlay.
//!
//! Window model:
//! * Overlay: full virtual-screen WS_POPUP, WS_EX_LAYERED | WS_EX_NOREDIRECTIONBITMAP | TOPMOST.
//!   Pixels come from a DirectComposition premultiplied-alpha swap chain.
//!   Draw mode: overlay receives all pointer input (hit-test covers the whole rect).
//!   Pass mode: WS_EX_TRANSPARENT is added; with WS_EX_LAYERED that makes the
//!   window invisible to hit testing, so input falls through to the desktop.
//! * Toolbar: separate non-layered popup owned by the overlay (so it always sits
//!   above it) and WS_EX_NOACTIVATE, so it stays clickable in both modes.

mod gfx;
mod layout;

use gfx::{OverlayGfx, Shape, Stroke, ToolbarGfx};
use layout::Button;
use std::cell::RefCell;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::Pointer::EnableMouseInPointer;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;
use windows_numerics::Vector2;

const OVERLAY_CLASS: PCWSTR = w!("AnnotatorOverlay");
const TOOLBAR_CLASS: PCWSTR = w!("AnnotatorToolbar");
const HOTKEY_TOGGLE: usize = 1;
const PEN_WIDTH_DIP: f32 = 4.0;
/// How far from a stroke a click still selects it.
const HIT_TOL_DIP: f32 = 6.0;
const TIMER_TOPMOST: usize = 1;

struct App {
    overlay: HWND,
    toolbar: HWND,
    d2d: ID2D1Factory1,
    ov: Option<OverlayGfx>,
    tb: Option<ToolbarGfx>,
    origin: (i32, i32),
    size: (i32, i32),
    monitors: Vec<RECT>,
    strokes: Vec<Stroke>,
    /// In-progress stroke and the pointer id drawing it.
    active: Option<(u32, Stroke)>,
    drawing: bool,
    color: usize,
    shape: Shape,
    /// Index into `strokes` picked with the Select tool.
    selected: Option<usize>,
    /// Pointer id moving the selection and its last position.
    dragging: Option<(u32, Vector2)>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the app state; returns None if the state is not yet created or is
/// already borrowed (a re-entrant message), in which case the caller falls back
/// to DefWindowProc instead of panicking.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok()?.as_mut().map(f))
}

/// Draw mode needs hit testing; pass mode adds WS_EX_TRANSPARENT to fall through.
fn overlay_ex_style(ex: u32, drawing: bool) -> u32 {
    if drawing { ex & !WS_EX_TRANSPARENT.0 } else { ex | WS_EX_TRANSPARENT.0 }
}

fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn monitor_rects() -> Vec<RECT> {
    unsafe extern "system" fn cb(_: HMONITOR, _: HDC, r: *mut RECT, out: LPARAM) -> BOOL {
        unsafe { (*(out.0 as *mut Vec<RECT>)).push(*r) };
        true.into()
    }
    let mut v = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

fn dpi_at(p: Vector2) -> f32 {
    let (mut x, mut y) = (96, 96);
    unsafe {
        let m = MonitorFromPoint(POINT { x: p.X as i32, y: p.Y as i32 }, MONITOR_DEFAULTTONEAREST);
        let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut x, &mut y);
    }
    x as f32
}

fn lparam_point(lp: LPARAM) -> (i32, i32) {
    ((lp.0 as i16) as i32, ((lp.0 >> 16) as i16) as i32)
}

impl App {
    fn render_overlay(&mut self) {
        // Second attempt recreates the device (e.g. after DXGI device removal).
        for _ in 0..2 {
            if self.ov.is_none() {
                self.ov = OverlayGfx::new(&self.d2d, self.overlay, self.size.0 as u32, self.size.1 as u32)
                    .inspect_err(|e| eprintln!("overlay gfx init failed: {e}"))
                    .ok();
            }
            let Some(g) = &self.ov else { return };
            let strokes = self.strokes.iter().chain(self.active.as_ref().map(|a| &a.1));
            let border = self.drawing.then_some(&self.monitors[..]);
            let sel = self.selected.and_then(|i| self.strokes.get(i)).map(Stroke::bounds);
            match g.render(self.origin, strokes, border, sel) {
                Ok(()) => return,
                Err(e) => {
                    eprintln!("overlay render failed, recreating: {e}");
                    self.ov = None;
                }
            }
        }
    }

    fn render_toolbar(&mut self) {
        for _ in 0..2 {
            if self.tb.is_none() {
                let dpi = unsafe { GetDpiForWindow(self.toolbar) };
                self.tb = ToolbarGfx::new(&self.d2d, self.toolbar, dpi)
                    .inspect_err(|e| eprintln!("toolbar gfx init failed: {e}"))
                    .ok();
            }
            let Some(g) = &self.tb else { return };
            match g.render(self.drawing, self.color, self.shape) {
                Ok(()) => return,
                Err(_) => self.tb = None, // D2DERR_RECREATE_TARGET and friends
            }
        }
    }

    fn set_mode(&mut self, drawing: bool) {
        self.drawing = drawing;
        if let Some((_, s)) = self.active.take() {
            self.strokes.push(s);
        }
        self.selected = None;
        self.dragging = None;
        unsafe {
            let ex = GetWindowLongW(self.overlay, GWL_EXSTYLE) as u32;
            SetWindowLongW(self.overlay, GWL_EXSTYLE, overlay_ex_style(ex, drawing) as i32);
            if drawing {
                // Take keyboard focus so Esc works; allowed because we just got input.
                let _ = SetForegroundWindow(self.overlay);
            }
            let _ = InvalidateRect(Some(self.toolbar), None, false);
        }
        self.render_overlay();
    }

    fn on_pointer(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) {
        let id = (wp.0 & 0xFFFF) as u32;
        let (x, y) = lparam_point(lp); // screen coords, physical pixels (PMv2)
        let p = Vector2 { X: x as f32, Y: y as f32 };
        match msg {
            WM_POINTERDOWN if self.drawing && self.shape == Shape::Select && self.dragging.is_none() => {
                let tol = HIT_TOL_DIP * dpi_at(p) / 96.0;
                // Topmost (last drawn) stroke wins.
                self.selected = self.strokes.iter().rposition(|s| s.hit(p, tol));
                self.dragging = self.selected.map(|_| (id, p));
            }
            WM_POINTERUPDATE | WM_POINTERUP if self.dragging.is_some_and(|d| d.0 == id) => {
                let last = self.dragging.unwrap().1;
                if let Some(s) = self.selected.and_then(|i| self.strokes.get_mut(i)) {
                    s.translate(p.X - last.X, p.Y - last.Y);
                }
                self.dragging = (msg == WM_POINTERUPDATE).then_some((id, p));
            }
            WM_POINTERDOWN if self.drawing && self.shape != Shape::Select && self.active.is_none() => {
                let c = layout::COLORS[self.color];
                let width = PEN_WIDTH_DIP * dpi_at(p) / 96.0;
                self.active = Some((id, Stroke { shape: self.shape, color: gfx::rgb(c, 1.0), width, points: vec![p] }));
            }
            WM_POINTERUPDATE => match &mut self.active {
                Some((aid, s)) if *aid == id => {
                    if s.shape != Shape::Pen && s.points.len() == 2 {
                        s.points[1] = p; // shapes keep only [start, current end]
                    } else if s.points.last() != Some(&p) {
                        s.points.push(p);
                    }
                }
                _ => return,
            },
            WM_POINTERUP => match self.active.take() {
                Some((aid, s)) if aid == id => {
                    // A shape clicked without dragging is invisible; drop it so Undo isn't wasted on it.
                    if s.shape == Shape::Pen || s.points.len() == 2 {
                        self.strokes.push(s);
                    }
                }
                other => {
                    self.active = other;
                    return;
                }
            },
            _ => return,
        }
        self.render_overlay();
    }

    fn on_toolbar_click(&mut self, x_px: i32) {
        let scale = unsafe { GetDpiForWindow(self.toolbar) } as f32 / 96.0;
        match layout::button_at(x_px as f32 / scale) {
            Some(Button::Mode) => self.set_mode(!self.drawing),
            Some(Button::Color(i)) => {
                self.color = i;
                self.set_mode(true);
            }
            Some(Button::Shape(i)) => {
                self.shape = Shape::ALL[i];
                self.set_mode(true);
            }
            Some(Button::Undo) => {
                self.strokes.pop();
                self.selected = None;
                self.dragging = None;
                self.render_overlay();
            }
            Some(Button::Clear) => {
                self.strokes.clear();
                self.selected = None;
                self.dragging = None;
                self.render_overlay();
            }
            Some(Button::Exit) => unsafe {
                let _ = PostMessageW(Some(self.overlay), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            Some(Button::Grip) | None => {}
        }
        unsafe {
            let _ = InvalidateRect(Some(self.toolbar), None, false);
        }
    }

    fn on_display_change(&mut self) {
        let (x, y, w, h) = virtual_screen();
        self.origin = (x, y);
        self.size = (w, h);
        self.monitors = monitor_rects();
        unsafe {
            let _ = SetWindowPos(self.overlay, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        self.ov = None; // swap chain size changed
        self.render_overlay();
    }
}

extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = match msg {
        WM_POINTERDOWN | WM_POINTERUPDATE | WM_POINTERUP => with_app(|a| a.on_pointer(msg, wp, lp)).is_some(),
        WM_HOTKEY if wp.0 == HOTKEY_TOGGLE => with_app(|a| a.set_mode(!a.drawing)).is_some(),
        WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => with_app(|a| a.set_mode(false)).is_some(),
        WM_DISPLAYCHANGE => with_app(|a| a.on_display_change()).is_some(),
        // Other topmost windows (taskbar, other always-on-top apps) can rise above
        // us when activated; keep re-asserting so the ink stays on top.
        // shortcut: Start menu and similar shell UI still win; beating them needs a signed UIAccess build.
        WM_TIMER if wp.0 == TIMER_TOPMOST => unsafe {
            let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            if let Some(tb) = with_app(|a| a.toolbar) {
                let _ = SetWindowPos(tb, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            }
            true
        },
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT && with_app(|a| a.shape == Shape::Select) == Some(true) => unsafe {
            if let Ok(c) = LoadCursorW(None, IDC_SIZEALL) {
                SetCursor(Some(c));
            }
            true
        },
        // The overlay spans every monitor; never let a DPI change resize it.
        WM_DPICHANGED => true,
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            true
        }
        _ => false,
    };
    if handled { LRESULT(0) } else { unsafe { DefWindowProcW(hwnd, msg, wp, lp) } }
}

extern "system" fn toolbar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCHITTEST => {
                let (sx, sy) = lparam_point(lp);
                let mut p = POINT { x: sx, y: sy };
                let _ = ScreenToClient(hwnd, &mut p);
                let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
                if layout::button_at(p.x as f32 / scale) == Some(Button::Grip) {
                    return LRESULT(HTCAPTION as isize);
                }
            }
            WM_LBUTTONDOWN => {
                let x = lparam_point(lp).0;
                if with_app(|a| a.on_toolbar_click(x)).is_some() {
                    return LRESULT(0);
                }
            }
            WM_PAINT => {
                with_app(|a| a.render_toolbar());
                let _ = ValidateRect(Some(hwnd), None);
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                with_app(|a| a.tb = None);
                return LRESULT(0);
            }
            WM_SIZE => {
                with_app(|a| a.tb = None);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn run() -> Result<()> {
    unsafe {
        // Physical-pixel coordinates everywhere; toolbar scales itself per monitor.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
        // Mouse arrives as WM_POINTER* too, so pen/touch/mouse share one path.
        EnableMouseInPointer(true)?;

        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        for (class, proc, cursor) in [
            (OVERLAY_CLASS, Some(overlay_proc as unsafe extern "system" fn(_, _, _, _) -> _), IDC_CROSS),
            (TOOLBAR_CLASS, Some(toolbar_proc as unsafe extern "system" fn(_, _, _, _) -> _), IDC_ARROW),
        ] {
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: proc,
                hInstance: instance,
                hCursor: LoadCursorW(None, cursor)?,
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                return Err(Error::from_thread());
            }
        }

        let (vx, vy, vw, vh) = virtual_screen();
        let overlay = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            OVERLAY_CLASS,
            w!("Annotator"),
            WS_POPUP,
            vx,
            vy,
            vw,
            vh,
            None,
            None,
            Some(instance),
            None,
        )?;
        // A layered window is not shown until its attributes are set. Constant
        // alpha 255 leaves the DirectComposition per-pixel alpha in charge.
        SetLayeredWindowAttributes(overlay, COLORREF(0), 255, LWA_ALPHA)?;

        let toolbar = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            TOOLBAR_CLASS,
            w!("Annotator Toolbar"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            Some(overlay), // owner => always above the overlay
            None,
            Some(instance),
            None,
        )?;
        let pref = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            toolbar,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as _,
            size_of_val(&pref) as u32,
        );
        let scale = GetDpiForWindow(toolbar) as f32 / 96.0;
        let (tw, th) = ((layout::width() * scale).round() as i32, (layout::HEIGHT * scale).round() as i32);
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        SetWindowPos(toolbar, None, (screen_w - tw) / 2, (12.0 * scale) as i32, tw, th, SWP_NOZORDER | SWP_NOACTIVATE)?;

        if let Err(e) = RegisterHotKey(Some(overlay), HOTKEY_TOGGLE as i32, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, 'D' as u32) {
            eprintln!("Ctrl+Shift+D hotkey unavailable (in use?): {e}");
        }

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                overlay,
                toolbar,
                d2d: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
                ov: None,
                tb: None,
                origin: (vx, vy),
                size: (vw, vh),
                monitors: monitor_rects(),
                strokes: Vec::new(),
                active: None,
                drawing: true,
                color: 0,
                shape: Shape::Pen,
                selected: None,
                dragging: None,
            });
            Ok::<_, Error>(())
        })?;

        let _ = ShowWindow(overlay, SW_SHOWNOACTIVATE);
        let _ = ShowWindow(toolbar, SW_SHOWNOACTIVATE);
        with_app(|a| a.set_mode(true));
        SetTimer(Some(overlay), TIMER_TOPMOST, 500, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        APP.with(|a| a.borrow_mut().take()); // release COM objects before exit
        Ok(())
    }
}

fn main() {
    if let Err(e) = run() {
        let text = HSTRING::from(format!("Annotator failed to start:\n{e}"));
        unsafe { MessageBoxW(None, &text, w!("Annotator"), MB_ICONERROR) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_toggles_only_transparent_bit() {
        let base = (WS_EX_LAYERED | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST | WS_EX_TOOLWINDOW).0;
        let pass = overlay_ex_style(base, false);
        assert_eq!(pass, base | WS_EX_TRANSPARENT.0);
        assert_eq!(overlay_ex_style(pass, true), base);
        assert_eq!(overlay_ex_style(pass, false), pass);
    }
}
