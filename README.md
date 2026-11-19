# Annotator

Draw on top of your Windows desktop. A native Rust app using Win32, Direct2D, DirectWrite and DirectComposition, with no web technology.

## Use

```
cargo run --release
```

- **Draw mode** (orange border, toolbar shows *Draw*): the mouse, pen or touch draws ink.
- **Pass mode** (toolbar shows *Pass*): your ink stays on screen and every click reaches the windows underneath, even clicks on top of the ink.
- **Select tool**: click on any line, shape or pen stroke to select it (an orange box appears), then drag to move it. Rectangles and ovals are picked by their outline, not their inside. Click empty space to deselect.
- **Eraser tool**: click or drag across ink to delete every stroke or shape it touches (whole strokes, not parts of them).
- The overlay re-asserts itself as topmost every 500 ms, so ink stays above other always-on-top windows and the taskbar.
- Switch modes with **Ctrl+Shift+D**, the toolbar's Mode button, or **Esc** (which only leaves draw mode).
- Toolbar: drag it by the grip on the left. Its buttons are tools (Select, Eraser, Pen, Line, Arrow, Rectangle, Oval; the active one is highlighted), colors (choosing a shape or a color switches to draw mode), Undo, Clear and Exit.

## How it works

| Concern | Approach |
|---|---|
| Transparency | The window uses `WS_EX_NOREDIRECTIONBITMAP`. A DirectComposition visual holds a swap chain with `DXGI_ALPHA_MODE_PREMULTIPLIED`, and DWM blends it pixel by pixel. |
| Click-through | The window is `WS_EX_LAYERED` (alpha 255). Pass mode adds `WS_EX_TRANSPARENT`, which drops the window from hit testing. `WS_EX_TRANSPARENT` alone does not do this, which is why the window must also be layered. |
| Input | `EnableMouseInPointer` routes mouse, pen and touch through `WM_POINTER*`. Each stroke follows one pointer id. |
| Toolbar | A separate non-layered popup, `WS_EX_NOACTIVATE`, owned by the overlay so it always stays above it. It is drawn with Direct2D and DirectWrite. |
| DPI | The app is Per-Monitor-V2 aware. The overlay works in physical pixels and ignores `WM_DPICHANGED`. The toolbar draws in DIPs and rescales on `WM_DPICHANGED`. Pen width scales with the DPI of the monitor where the stroke starts. |
| Device loss | If rendering fails, the D2D/D3D/DComp stack is rebuilt once and the frame is redrawn. |

## Tests

```
cargo test                                                  # unit tests
cargo test --test overlay_e2e -- --ignored --nocapture      # real-desktop end-to-end test
```

The end-to-end test starts the real binary and sends real input with `SendInput`. It then reads composited screen pixels to check that:
the overlay is transparent; draw mode captures clicks and shows ink; the hotkey switches to pass mode; ink stays after the switch; clicks pass through both ink and empty areas; the toolbar takes clicks in pass mode; Clear works; and Exit works.
It needs an unlocked desktop and moves your mouse for about 5 seconds.

### Still to verify by hand (not automated)

- [ ] Mixed-DPI multi-monitor setup: ink lines up on every monitor, and the toolbar rescales when dragged across monitors
- [ ] Pen and touch input (only the mouse is tested)
- [ ] Hot-plugging or rearranging monitors (`WM_DISPLAYCHANGE` path)
- [ ] Device loss (for example a GPU driver update while running)
- [ ] Above full-screen exclusive games or apps (expected **not** to work; DWM overlays cannot cover exclusive full-screen)
