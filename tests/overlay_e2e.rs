//! End-to-end check of the real overlay on a real desktop: launches the binary,
//! injects genuine mouse/keyboard input with SendInput, and reads composed screen
//! pixels back. Moves your mouse for a few seconds.
//!
//! Run: cargo test --test overlay_e2e -- --ignored --nocapture
//! Needs an unlocked interactive desktop; don't touch mouse/keyboard while it runs.

#[path = "../src/layout.rs"]
#[allow(dead_code)]
mod layout;

use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

static TARGET_CLICKS: AtomicU32 = AtomicU32::new(0);
const BLUE: u32 = 0x00FF_0000; // COLORREF 0x00BBGGRR

extern "system" fn target_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if m == WM_LBUTTONDOWN {
        TARGET_CLICKS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe { DefWindowProcW(h, m, w, l) }
}

fn pump(ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn pixel(x: i32, y: i32) -> (u8, u8, u8) {
    unsafe {
        let dc = GetDC(None);
        let c = GetPixel(dc, x, y).0;
        ReleaseDC(None, dc);
        (c as u8, (c >> 8) as u8, (c >> 16) as u8)
    }
}

fn is_ink(p: (u8, u8, u8)) -> bool {
    p.0 > 200 && p.1 < 90 && p.2 < 90 // default pen is red
}

fn send(inputs: &[INPUT]) {
    unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
}

fn mouse(x: i32, y: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: ((x - vx) * 65535 + (vw - 1) / 2) / (vw - 1),
                dy: ((y - vy) * 65535 + (vh - 1) / 2) / (vh - 1),
                dwFlags: flags | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ..Default::default()
            },
        },
    }
}

fn click(x: i32, y: i32) {
    send(&[mouse(x, y, MOUSE_EVENT_FLAGS(0))]);
    pump(60);
    send(&[mouse(x, y, MOUSEEVENTF_LEFTDOWN)]);
    pump(40);
    send(&[mouse(x, y, MOUSEEVENTF_LEFTUP)]);
    pump(250);
}

fn drag(x0: i32, x1: i32, y: i32) {
    send(&[mouse(x0, y, MOUSE_EVENT_FLAGS(0))]);
    pump(60);
    send(&[mouse(x0, y, MOUSEEVENTF_LEFTDOWN)]);
    pump(40);
    for i in 1..=20 {
        send(&[mouse(x0 + (x1 - x0) * i / 20, y, MOUSE_EVENT_FLAGS(0))]);
        pump(15);
    }
    send(&[mouse(x1, y, MOUSEEVENTF_LEFTUP)]);
    pump(300);
}

fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) }, ..Default::default() },
        },
    }
}

fn hotkey_toggle() {
    let d = VIRTUAL_KEY('D' as u16);
    send(&[key(VK_CONTROL, false), key(VK_SHIFT, false), key(d, false), key(d, true), key(VK_SHIFT, true), key(VK_CONTROL, true)]);
    pump(300);
}

fn passthrough(overlay: HWND) -> bool {
    unsafe { GetWindowLongW(overlay, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT.0 != 0 }
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn wait_window(class: PCWSTR) -> HWND {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(h) = unsafe { FindWindowW(class, None) } {
            return h;
        }
        assert!(Instant::now() < end, "window class never appeared");
        pump(50);
    }
}

#[test]
#[ignore = "drives the real desktop; run with --ignored"]
fn overlay_transparency_clickthrough_and_toolbar() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).unwrap();
        assert!(FindWindowW(w!("AnnotatorOverlay"), None).is_err(), "close the running annotator first");

        // A solid blue topmost target on the primary monitor, created before the
        // overlay so the overlay (also topmost, created later) sits above it.
        let inst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(target_proc),
            hInstance: inst,
            hbrBackground: CreateSolidBrush(COLORREF(BLUE)),
            lpszClassName: w!("AnnotatorE2ETarget"),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let (cx, cy) = (sw / 2, sh / 2);
        let target = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("AnnotatorE2ETarget"),
            w!("target"),
            WS_POPUP | WS_VISIBLE,
            cx - 300,
            cy - 200,
            600,
            400,
            None,
            None,
            Some(inst),
            None,
        )
        .unwrap();
        pump(500);
        assert_eq!(pixel(cx, cy), (0, 0, 255), "target not visible before overlay starts");

        let _app = Kill(Command::new(env!("CARGO_BIN_EXE_annotator")).spawn().unwrap());
        let overlay = wait_window(w!("AnnotatorOverlay"));
        let toolbar = wait_window(w!("AnnotatorToolbar"));
        pump(800);

        // 1. Starts in draw mode; overlay renders (border on primary monitor)
        //    yet is transparent elsewhere.
        assert!(!passthrough(overlay), "should start in draw mode");
        let b = pixel(1, 1);
        assert!(b.0 > 200 && b.1 > 90 && b.1 < 190 && b.2 < 80, "draw-mode border not rendered: {b:?}");
        assert_eq!(pixel(cx, cy), (0, 0, 255), "overlay is not transparent");

        // 2. Draw mode captures input: dragging draws ink, target sees no click.
        drag(cx - 150, cx + 150, cy);
        assert_eq!(TARGET_CLICKS.load(Ordering::SeqCst), 0, "draw mode leaked a click");
        assert!(is_ink(pixel(cx, cy)), "stroke not visible: {:?}", pixel(cx, cy));
        assert_eq!(pixel(cx, cy + 60), (0, 0, 255), "area off the stroke not transparent");

        // 3. Hotkey -> pass-through: ink persists, border goes, clicks fall through,
        //    including clicks directly on the ink.
        hotkey_toggle();
        assert!(passthrough(overlay), "hotkey did not enter pass-through");
        assert!(is_ink(pixel(cx, cy)), "ink lost in pass-through");
        let b = pixel(1, 1);
        assert!(!(b.0 > 200 && b.1 > 90 && b.1 < 190 && b.2 < 80), "border still shown in pass-through: {b:?}");
        click(cx, cy);
        assert_eq!(TARGET_CLICKS.load(Ordering::SeqCst), 1, "click on ink did not pass through");
        click(cx, cy + 60);
        assert_eq!(TARGET_CLICKS.load(Ordering::SeqCst), 2, "click on empty overlay did not pass through");

        // 4. Toolbar stays interactive in pass-through mode.
        let mut tr = RECT::default();
        GetWindowRect(toolbar, &mut tr).unwrap();
        let scale = GetDpiForWindow(toolbar) as f32 / 96.0;
        let at = |b| {
            let (x, y) = layout::center_of(b).unwrap();
            (tr.left + (x * scale) as i32, tr.top + (y * scale) as i32)
        };
        let (mx, my) = at(layout::Button::Mode);
        click(mx, my);
        assert!(!passthrough(overlay), "toolbar Mode click did not reach toolbar");
        assert_eq!(TARGET_CLICKS.load(Ordering::SeqCst), 2);

        // 5. Clear removes ink; Exit closes the app.
        let (x, y) = at(layout::Button::Clear);
        click(x, y);
        assert_eq!(pixel(cx, cy), (0, 0, 255), "Clear did not remove ink");
        let (x, y) = at(layout::Button::Exit);
        click(x, y);
        pump(1000);
        assert!(FindWindowW(w!("AnnotatorOverlay"), None).is_err(), "Exit did not close the app");

        let _ = DestroyWindow(target);
    }
}
