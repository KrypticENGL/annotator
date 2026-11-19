//! Direct2D rendering for the overlay (via a DirectComposition swap chain with
//! premultiplied alpha, so DWM blends it per-pixel over the desktop) and for the
//! toolbar (plain HWND render target + DirectWrite).

use crate::layout::{self, Button};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::*;
use windows_numerics::{Matrix3x2, Vector2};

pub struct Stroke {
    pub color: D2D1_COLOR_F,
    pub width: f32,
    /// Screen coordinates in physical pixels.
    pub points: Vec<Vector2>,
}

pub fn rgb(c: [f32; 3], a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c[0], g: c[1], b: c[2], a }
}

const BORDER: D2D1_COLOR_F = D2D1_COLOR_F { r: 1.0, g: 0.55, b: 0.0, a: 0.9 };
const CLEAR: D2D1_COLOR_F = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

pub struct OverlayGfx {
    swap: IDXGISwapChain1,
    ctx: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    round: ID2D1StrokeStyle1,
    // Kept alive: dropping these detaches the swap chain from the window.
    _dcomp: (IDCompositionDevice, IDCompositionTarget, IDCompositionVisual),
}

impl OverlayGfx {
    pub fn new(factory: &ID2D1Factory1, hwnd: HWND, w: u32, h: u32) -> Result<Self> {
        unsafe {
            let mut d3d = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut d3d),
                None,
                None,
            )?;
            let dxgi: IDXGIDevice = d3d.unwrap().cast()?;

            let swap = CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0))?
                .CreateSwapChainForComposition(
                    &dxgi,
                    &DXGI_SWAP_CHAIN_DESC1 {
                        Width: w,
                        Height: h,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                        BufferCount: 2,
                        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                        ..Default::default()
                    },
                    None,
                )?;

            let ctx = factory
                .CreateDevice(&dxgi)?
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let surface: IDXGISurface = swap.GetBuffer(0)?;
            let target = ctx.CreateBitmapFromDxgiSurface(
                &surface,
                Some(&D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    // 96 DPI => 1 DIP == 1 physical pixel; we work in pixels.
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    ..Default::default()
                }),
            )?;
            ctx.SetTarget(&target);
            ctx.SetDpi(96.0, 96.0);

            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let dtarget = dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            dtarget.SetRoot(&visual)?;
            dcomp.Commit()?;

            let brush = ctx.CreateSolidColorBrush(&BORDER, None)?;
            let round = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES1 {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    ..Default::default()
                },
                None,
            )?;
            Ok(Self { swap, ctx, brush, round, _dcomp: (dcomp, dtarget, visual) })
        }
    }

    /// `origin` is the virtual-screen origin (window top-left in screen coords).
    /// `border` lists monitor rects (screen coords) to outline as a draw-mode cue.
    pub fn render<'a>(
        &self,
        origin: (i32, i32),
        strokes: impl Iterator<Item = &'a Stroke>,
        border: Option<&[RECT]>,
    ) -> Result<()> {
        unsafe {
            let ctx = &self.ctx;
            ctx.BeginDraw();
            ctx.Clear(Some(&CLEAR));
            ctx.SetTransform(&Matrix3x2::translation(-origin.0 as f32, -origin.1 as f32));
            // shortcut: redraws every stroke each frame; cache committed ink in a bitmap if this gets slow.
            for s in strokes {
                self.brush.SetColor(&s.color);
                if let [p] = s.points[..] {
                    let r = s.width / 2.0;
                    ctx.FillEllipse(&D2D1_ELLIPSE { point: p, radiusX: r, radiusY: r }, &self.brush);
                }
                for w in s.points.windows(2) {
                    ctx.DrawLine(w[0], w[1], &self.brush, s.width, &self.round);
                }
            }
            if let Some(mons) = border {
                self.brush.SetColor(&BORDER);
                for m in mons {
                    let r = D2D_RECT_F {
                        left: m.left as f32 + 2.0,
                        top: m.top as f32 + 2.0,
                        right: m.right as f32 - 2.0,
                        bottom: m.bottom as f32 - 2.0,
                    };
                    ctx.DrawRectangle(&r, &self.brush, 4.0, None);
                }
            }
            ctx.EndDraw(None, None)?;
            self.swap.Present(1, DXGI_PRESENT(0)).ok()
        }
    }
}

pub struct ToolbarGfx {
    rt: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    text: IDWriteTextFormat,
}

impl ToolbarGfx {
    pub fn new(factory: &ID2D1Factory1, hwnd: HWND, dpi: u32) -> Result<Self> {
        unsafe {
            let mut rc = RECT::default();
            windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rc)?;
            let rt = factory.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES::default(),
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd,
                    pixelSize: D2D_SIZE_U { width: rc.right as u32, height: rc.bottom as u32 },
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )?;
            // Draw in DIPs; D2D scales to the monitor's DPI.
            rt.SetDpi(dpi as f32, dpi as f32);
            let brush = rt.CreateSolidColorBrush(&CLEAR, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let text = dw.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                13.0,
                w!("en-us"),
            )?;
            text.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            text.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            Ok(Self { rt, brush, text })
        }
    }

    pub fn render(&self, drawing: bool, color: usize) -> Result<()> {
        let gray = rgb([0.24, 0.25, 0.27], 1.0);
        let white = rgb([0.95, 0.95, 0.95], 1.0);
        unsafe {
            let rt = &self.rt;
            rt.BeginDraw();
            rt.Clear(Some(&rgb([0.12, 0.13, 0.14], 1.0)));
            for (b, x0, x1) in layout::items() {
                let r = D2D_RECT_F { left: x0 + 4.0, top: 4.0, right: x1 - 4.0, bottom: layout::HEIGHT - 4.0 };
                let label = match b {
                    Button::Grip => {
                        self.brush.SetColor(&gray);
                        for i in 0..3 {
                            let y = 14.0 + i as f32 * 6.0;
                            rt.DrawLine(Vector2 { X: 6.0, Y: y }, Vector2 { X: 12.0, Y: y }, &self.brush, 2.0, None);
                        }
                        continue;
                    }
                    Button::Color(i) => {
                        let e = D2D1_ELLIPSE {
                            point: Vector2 { X: (x0 + x1) / 2.0, Y: layout::HEIGHT / 2.0 },
                            radiusX: 10.0,
                            radiusY: 10.0,
                        };
                        self.brush.SetColor(&rgb(layout::COLORS[i], 1.0));
                        rt.FillEllipse(&e, &self.brush);
                        if i == color {
                            self.brush.SetColor(&white);
                            rt.DrawEllipse(&e, &self.brush, 2.5, None);
                        }
                        continue;
                    }
                    Button::Mode if drawing => {
                        self.brush.SetColor(&rgb([1.0, 0.55, 0.0], 1.0));
                        "Draw"
                    }
                    Button::Mode => {
                        self.brush.SetColor(&gray);
                        "Pass"
                    }
                    Button::Undo => {
                        self.brush.SetColor(&gray);
                        "Undo"
                    }
                    Button::Clear => {
                        self.brush.SetColor(&gray);
                        "Clear"
                    }
                    Button::Exit => {
                        self.brush.SetColor(&rgb([0.55, 0.15, 0.15], 1.0));
                        "Exit"
                    }
                };
                rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 5.0, radiusY: 5.0 }, &self.brush);
                self.brush.SetColor(&white);
                let wide: Vec<u16> = label.encode_utf16().collect();
                rt.DrawText(
                    &wide,
                    &self.text,
                    &r,
                    &self.brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
            rt.EndDraw(None, None)
        }
    }
}
