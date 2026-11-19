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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    /// The selection tool; never stored in a `Stroke`.
    Select,
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
}

impl Shape {
    /// Toolbar order; `Button::Shape(i)` indexes this.
    pub const ALL: [Shape; 6] = [Shape::Select, Shape::Pen, Shape::Line, Shape::Arrow, Shape::Rect, Shape::Ellipse];

    /// Toolbar icon geometry around (cx, cy), in DIPs.
    fn icon_points(self, cx: f32, cy: f32) -> Vec<Vector2> {
        let pts: &[(f32, f32)] = match self {
            // Mouse-pointer outline, drawn as a Pen polyline.
            Shape::Select => &[(-4.0, -8.0), (-4.0, 7.0), (0.0, 3.0), (3.0, 9.0), (5.0, 8.0), (2.0, 2.0), (7.0, 2.0), (-4.0, -8.0)],
            Shape::Pen => &[(-8.0, 4.0), (-3.0, -4.0), (2.0, 4.0), (7.0, -4.0)],
            Shape::Line | Shape::Arrow => &[(-7.0, 7.0), (7.0, -7.0)],
            Shape::Rect | Shape::Ellipse => &[(-8.0, -6.0), (8.0, 6.0)],
        };
        pts.iter().map(|&(x, y)| Vector2 { X: cx + x, Y: cy + y }).collect()
    }
}

pub struct Stroke {
    pub shape: Shape,
    pub color: D2D1_COLOR_F,
    pub width: f32,
    /// Screen coordinates in physical pixels. Shapes other than Pen hold [start, end].
    pub points: Vec<Vector2>,
}

impl Stroke {
    /// The drawn outline as line segments; used for hit testing and bounds.
    fn segments(&self) -> Vec<(Vector2, Vector2)> {
        let v = |x, y| Vector2 { X: x, Y: y };
        let chain = |pts: &[Vector2]| pts.windows(2).map(|w| (w[0], w[1])).collect();
        match (self.shape, &self.points[..]) {
            (Shape::Pen, &[p]) => vec![(p, p)],
            (Shape::Pen, pts) => chain(pts),
            (Shape::Line, &[a, b]) => vec![(a, b)],
            (Shape::Arrow, &[a, b]) => {
                let wings = arrow_wings(a, b, self.width * 4.0);
                std::iter::once((a, b)).chain(wings.into_iter().map(|w| (b, w))).collect()
            }
            (Shape::Rect, &[a, b]) => chain(&[a, v(b.X, a.Y), b, v(a.X, b.Y), a]),
            (Shape::Ellipse, &[a, b]) => {
                let (cx, cy) = ((a.X + b.X) / 2.0, (a.Y + b.Y) / 2.0);
                let (rx, ry) = ((b.X - a.X).abs() / 2.0, (b.Y - a.Y).abs() / 2.0);
                let pts: Vec<_> = (0..=32)
                    .map(|i| (i as f32 * std::f32::consts::TAU / 32.0).sin_cos())
                    .map(|(s, c)| v(cx + rx * c, cy + ry * s))
                    .collect();
                chain(&pts)
            }
            _ => vec![],
        }
    }

    /// True if `p` is within `tol` of the drawn ink (outline only, not the interior).
    pub fn hit(&self, p: Vector2, tol: f32) -> bool {
        let r = self.width / 2.0 + tol;
        self.segments().iter().any(|&(a, b)| seg_dist(p, a, b) <= r)
    }

    pub fn bounds(&self) -> D2D_RECT_F {
        let pad = self.width / 2.0;
        let mut r = D2D_RECT_F { left: f32::MAX, top: f32::MAX, right: f32::MIN, bottom: f32::MIN };
        for p in self.segments().into_iter().flat_map(|(a, b)| [a, b]) {
            r.left = r.left.min(p.X - pad);
            r.top = r.top.min(p.Y - pad);
            r.right = r.right.max(p.X + pad);
            r.bottom = r.bottom.max(p.Y + pad);
        }
        r
    }

    pub fn translate(&mut self, dx: f32, dy: f32) {
        for p in &mut self.points {
            p.X += dx;
            p.Y += dy;
        }
    }
}

/// Distance from `p` to the segment a-b.
fn seg_dist(p: Vector2, a: Vector2, b: Vector2) -> f32 {
    let (dx, dy) = (b.X - a.X, b.Y - a.Y);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p.X - a.X) * dx + (p.Y - a.Y) * dy) / len2).clamp(0.0, 1.0) };
    (p.X - a.X - t * dx).hypot(p.Y - a.Y - t * dy)
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
            let round = round_style(factory)?;
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
        selection: Option<D2D_RECT_F>,
    ) -> Result<()> {
        unsafe {
            let ctx = &self.ctx;
            ctx.BeginDraw();
            ctx.Clear(Some(&CLEAR));
            ctx.SetTransform(&Matrix3x2::translation(-origin.0 as f32, -origin.1 as f32));
            // shortcut: redraws every stroke each frame; cache committed ink in a bitmap if this gets slow.
            for s in strokes {
                self.brush.SetColor(&s.color);
                draw_stroke(ctx, s.shape, &s.points, s.width, &self.brush, &self.round);
            }
            if let Some(r) = selection {
                self.brush.SetColor(&BORDER);
                let r = D2D_RECT_F { left: r.left - 4.0, top: r.top - 4.0, right: r.right + 4.0, bottom: r.bottom + 4.0 };
                ctx.DrawRectangle(&r, &self.brush, 2.0, None);
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

/// Shared by the overlay ink and the toolbar's shape icons.
fn draw_stroke(
    rt: &ID2D1RenderTarget,
    shape: Shape,
    points: &[Vector2],
    width: f32,
    brush: &ID2D1SolidColorBrush,
    style: &ID2D1StrokeStyle,
) {
    unsafe {
        match (shape, points) {
            (Shape::Pen, &[p]) => {
                let r = width / 2.0;
                rt.FillEllipse(&D2D1_ELLIPSE { point: p, radiusX: r, radiusY: r }, brush);
            }
            (Shape::Pen, pts) => {
                for w in pts.windows(2) {
                    rt.DrawLine(w[0], w[1], brush, width, style);
                }
            }
            (Shape::Line, &[a, b]) => rt.DrawLine(a, b, brush, width, style),
            (Shape::Arrow, &[a, b]) => {
                rt.DrawLine(a, b, brush, width, style);
                for w in arrow_wings(a, b, width * 4.0) {
                    rt.DrawLine(b, w, brush, width, style);
                }
            }
            (Shape::Rect, &[a, b]) => {
                let r = D2D_RECT_F { left: a.X.min(b.X), top: a.Y.min(b.Y), right: a.X.max(b.X), bottom: a.Y.max(b.Y) };
                rt.DrawRectangle(&r, brush, width, style);
            }
            (Shape::Ellipse, &[a, b]) => {
                let e = D2D1_ELLIPSE {
                    point: Vector2 { X: (a.X + b.X) / 2.0, Y: (a.Y + b.Y) / 2.0 },
                    radiusX: (b.X - a.X).abs() / 2.0,
                    radiusY: (b.Y - a.Y).abs() / 2.0,
                };
                rt.DrawEllipse(&e, brush, width, style);
            }
            _ => {} // a shape that has only its start point yet
        }
    }
}

fn round_style(factory: &ID2D1Factory1) -> Result<ID2D1StrokeStyle1> {
    unsafe {
        factory.CreateStrokeStyle(
            &D2D1_STROKE_STYLE_PROPERTIES1 {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                ..Default::default()
            },
            None,
        )
    }
}

/// Tips of the two arrowhead strokes at `b`, each `len` long and 30° off the shaft.
fn arrow_wings(a: Vector2, b: Vector2, len: f32) -> Vec<Vector2> {
    let (dx, dy) = (a.X - b.X, a.Y - b.Y);
    let d = dx.hypot(dy);
    if d == 0.0 {
        return vec![];
    }
    let (ux, uy) = (dx / d * len, dy / d * len);
    let (sin, cos) = 30f32.to_radians().sin_cos();
    [sin, -sin].map(|s| Vector2 { X: b.X + ux * cos - uy * s, Y: b.Y + ux * s + uy * cos }).to_vec()
}

pub struct ToolbarGfx {
    rt: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    text: IDWriteTextFormat,
    round: ID2D1StrokeStyle1,
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
            Ok(Self { rt, brush, text, round: round_style(factory)? })
        }
    }

    pub fn render(&self, drawing: bool, color: usize, shape: Shape) -> Result<()> {
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
                    Button::Shape(i) => {
                        let s = Shape::ALL[i];
                        if s == shape {
                            self.brush.SetColor(&gray);
                            rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 5.0, radiusY: 5.0 }, &self.brush);
                        }
                        self.brush.SetColor(&white);
                        let pts = s.icon_points((x0 + x1) / 2.0, layout::HEIGHT / 2.0);
                        let kind = if s == Shape::Select { Shape::Pen } else { s };
                        draw_stroke(rt, kind, &pts, 2.0, &self.brush, &self.round);
                        continue;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_has_one_button_per_shape() {
        let idx: Vec<usize> = layout::items().filter_map(|(b, ..)| if let Button::Shape(i) = b { Some(i) } else { None }).collect();
        assert_eq!(idx, (0..Shape::ALL.len()).collect::<Vec<_>>());
    }

    fn stroke(shape: Shape, pts: &[(f32, f32)]) -> Stroke {
        let points = pts.iter().map(|&(x, y)| Vector2 { X: x, Y: y }).collect();
        Stroke { shape, color: CLEAR, width: 2.0, points }
    }

    #[test]
    fn hit_testing_follows_the_outline() {
        let p = |x, y| Vector2 { X: x, Y: y };
        let line = stroke(Shape::Line, &[(0.0, 0.0), (100.0, 0.0)]);
        assert!(line.hit(p(50.0, 5.0), 5.0));
        assert!(!line.hit(p(50.0, 7.0), 5.0));
        assert!(!line.hit(p(110.0, 0.0), 5.0));

        let rect = stroke(Shape::Rect, &[(100.0, 100.0), (0.0, 0.0)]);
        assert!(rect.hit(p(100.0, 50.0), 1.0));
        assert!(!rect.hit(p(50.0, 50.0), 1.0)); // interior is click-through

        let oval = stroke(Shape::Ellipse, &[(0.0, 0.0), (100.0, 50.0)]);
        assert!(oval.hit(p(50.0, 0.0), 1.0) && oval.hit(p(100.0, 25.0), 1.0));
        assert!(!oval.hit(p(50.0, 25.0), 1.0));

        let dot = stroke(Shape::Pen, &[(10.0, 10.0)]);
        assert!(dot.hit(p(12.0, 10.0), 1.0));
    }

    #[test]
    fn translate_moves_bounds() {
        let mut s = stroke(Shape::Rect, &[(10.0, 20.0), (30.0, 40.0)]);
        s.translate(5.0, -5.0);
        let b = s.bounds();
        assert_eq!((b.left, b.top, b.right, b.bottom), (14.0, 14.0, 36.0, 36.0));
    }

    #[test]
    fn arrow_wings_point_back_symmetrically() {
        let w = arrow_wings(Vector2 { X: 0.0, Y: 0.0 }, Vector2 { X: 10.0, Y: 0.0 }, 2.0);
        assert_eq!(w.len(), 2);
        for p in &w {
            assert!(p.X < 10.0 && ((p.X - 10.0).hypot(p.Y) - 2.0).abs() < 1e-4);
        }
        assert!((w[0].Y + w[1].Y).abs() < 1e-4);
        assert!(arrow_wings(Vector2 { X: 1.0, Y: 1.0 }, Vector2 { X: 1.0, Y: 1.0 }, 2.0).is_empty());
    }
}
