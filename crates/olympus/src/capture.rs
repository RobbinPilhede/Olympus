use olympus_core::config::home_dir;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FrameRect, GetDC, GetDIBits, GetMonitorInfoW, GetStockObject, InvalidateRect, MonitorFromPoint, ReleaseDC, RoundRect, SelectObject, SetBkMode, SetDIBitsToDevice, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, DRAW_TEXT_FORMAT, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, FONT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY, HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST, NULL_PEN, PAINTSTRUCT, SRCCOPY, TRANSPARENT};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, LoadCursorW, PostQuitMessage, RegisterClassW, SetCursor, SetForegroundWindow, ShowWindow, TranslateMessage, IDC_ARROW, IDC_CROSS, MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP};

const ESCAPE: usize = 0x1B;
const ENTER: usize = 0x0D;
const LETTER_Z: usize = 0x5A;
const SMALLEST_DRAG: i32 = 5;
const PENCIL: [u8; 4] = [48, 59, 255, 255];
const HINT: &str = "Draw with the mouse";
const HINT_WIDTH: i32 = 146;
const TOOLS: [(Tool, &str, i32); 3] = [(Tool::Undo, "Undo", 64), (Tool::Cancel, "Cancel", 72), (Tool::Send, "Send to Olympus", 148)];
const KEEP_FOR: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

static OPEN: AtomicBool = AtomicBool::new(false);

/// Where screenshots and pasted images wait until they've been sent.
pub fn attachments_dir() -> PathBuf {
    home_dir().join("attachments")
}

/// Removes attachments old enough that no chat is still waiting on them.
pub fn prune_attachments(dir: &Path, keep_for: std::time::Duration) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let old = entry.metadata().and_then(|meta| meta.modified()).ok().and_then(|at| at.elapsed().ok()).is_some_and(|age| age > keep_for);
        if old && entry.path().is_file() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The files copied in Explorer, if that's what the clipboard holds.
pub fn clipboard_files() -> Vec<PathBuf> {
    const CF_HDROP: u32 = 15;
    let mut files = Vec::new();
    // SAFETY: the clipboard is opened and closed here, and the handle is only used while it's open.
    unsafe {
        if OpenClipboard(None).is_err() {
            return files;
        }
        if let Ok(handle) = GetClipboardData(CF_HDROP) {
            let drop = HDROP(handle.0);
            for index in 0..DragQueryFileW(drop, u32::MAX, None) {
                let mut name = vec![0u16; DragQueryFileW(drop, index, None) as usize + 1];
                let length = DragQueryFileW(drop, index, Some(&mut name)) as usize;
                files.push(PathBuf::from(String::from_utf16_lossy(&name[..length])));
            }
        }
        let _ = CloseClipboard();
    }
    files
}

/// A new file name that sorts by time and can't collide.
pub fn new_attachment_path(extension: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis());
    attachments_dir().join(format!("shot-{stamp}.{extension}"))
}

type Area = (i32, i32, i32, i32);

/// A rectangle from any two corners of a drag.
pub fn drag_rect(from: (i32, i32), to: (i32, i32)) -> Area {
    (from.0.min(to.0), from.1.min(to.1), from.0.max(to.0), from.1.max(to.1))
}

/// Copies a rectangle out of top-down BGRA pixels as opaque RGBA.
pub fn crop_rgba(pixels: &[u8], width: i32, height: i32, rect: (i32, i32, i32, i32)) -> (u32, u32, Vec<u8>) {
    let left = rect.0.clamp(0, width);
    let top = rect.1.clamp(0, height);
    let right = rect.2.clamp(left, width);
    let bottom = rect.3.clamp(top, height);
    let mut out = Vec::with_capacity(((right - left) * (bottom - top) * 4) as usize);
    for y in top..bottom {
        let start = ((y * width + left) * 4) as usize;
        let end = ((y * width + right) * 4) as usize;
        for pixel in pixels[start..end].chunks_exact(4) {
            out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    ((right - left) as u32, (bottom - top) as u32, out)
}

/// Halves the brightness of BGRA pixels, for what isn't selected.
pub fn dimmed(pixels: &[u8]) -> Vec<u8> {
    pixels.chunks_exact(4).flat_map(|pixel| [pixel[0] / 2, pixel[1] / 2, pixel[2] / 2, 255]).collect()
}

/// Draws a round-tipped line into top-down BGRA pixels, staying inside `clip`.
fn pencil_line(pixels: &mut [u8], width: i32, height: i32, clip: Area, from: (i32, i32), to: (i32, i32), radius: i32) {
    let steps = (to.0 - from.0).abs().max((to.1 - from.1).abs()).max(1);
    for step in 0..=steps {
        let center = (from.0 + (to.0 - from.0) * step / steps, from.1 + (to.1 - from.1) * step / steps);
        for y in (center.1 - radius).max(clip.1).max(0)..(center.1 + radius + 1).min(clip.3).min(height) {
            for x in (center.0 - radius).max(clip.0).max(0)..(center.0 + radius + 1).min(clip.2).min(width) {
                if (x - center.0).pow(2) + (y - center.1).pow(2) <= radius * radius {
                    let start = ((y * width + x) * 4) as usize;
                    pixels[start..start + 4].copy_from_slice(&PENCIL);
                }
            }
        }
    }
}

/// Writes opaque RGBA pixels as a PNG.
pub fn save_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().and_then(|mut writer| writer.write_image_data(rgba)).map_err(|error| error.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tool {
    Undo,
    Cancel,
    Send,
}

#[derive(Debug, PartialEq)]
struct Toolbar {
    area: Area,
    hint: Area,
    buttons: Vec<(Tool, &'static str, Area)>,
    dpi: i32,
}

impl Toolbar {
    // Below the picture, or above it, or inside it when it fills the screen.
    fn new(region: Area, monitor: Area, dpi: i32) -> Self {
        let scale = |value: i32| value * dpi / 96;
        let (pad, gap, height, button) = (scale(6), scale(4), scale(40), scale(28));
        let width = pad * 2 + scale(HINT_WIDTH) + TOOLS.iter().map(|tool| scale(tool.2) + gap).sum::<i32>();
        let left = (region.2 - width).min(monitor.2 - width).max(monitor.0);
        let top = if region.3 + scale(8) + height <= monitor.3 {
            region.3 + scale(8)
        } else if region.1 - scale(8) - height >= monitor.1 {
            region.1 - scale(8) - height
        } else {
            region.3 - scale(8) - height
        };
        let mut next = left + pad + scale(HINT_WIDTH) + gap;
        let inner = top + (height - button) / 2;
        let buttons = TOOLS
            .iter()
            .map(|&(tool, label, tool_width)| {
                let area = (next, inner, next + scale(tool_width), inner + button);
                next = area.2 + gap;
                (tool, label, area)
            })
            .collect();
        Self { area: (left, top, left + width, top + height), hint: (left + scale(14), top, left + pad + scale(HINT_WIDTH), top + height), buttons, dpi }
    }

    fn tool_at(&self, at: (i32, i32)) -> Option<Tool> {
        self.buttons.iter().find(|button| inside(button.2, at)).map(|button| button.0)
    }
}

fn inside(area: Area, at: (i32, i32)) -> bool {
    at.0 >= area.0 && at.0 < area.2 && at.1 >= area.1 && at.1 < area.3
}

pub(crate) struct Shot {
    pub(crate) pixels: Vec<u8>,
    dim: Vec<u8>,
    clean: Vec<u8>,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) origin: (i32, i32),
    start: Option<(i32, i32)>,
    current: (i32, i32),
    chosen: Option<Area>,
    toolbar: Option<Toolbar>,
    hover: Option<Tool>,
    strokes: Vec<Vec<(i32, i32)>>,
    drawing: bool,
    send: bool,
}

impl Shot {
    fn new(pixels: Vec<u8>, width: i32, height: i32, origin: (i32, i32)) -> Self {
        let dim = dimmed(&pixels);
        Self { pixels, dim, clean: Vec::new(), width, height, origin, start: None, current: (0, 0), chosen: None, toolbar: None, hover: None, strokes: Vec::new(), drawing: false, send: false }
    }

    fn choose(&mut self, region: Area, monitor: Area, dpi: i32) {
        self.chosen = Some(region);
        self.toolbar = Some(Toolbar::new(region, monitor, dpi));
        self.clean.clone_from(&self.pixels);
    }

    fn radius(&self) -> i32 {
        self.toolbar.as_ref().map_or(2, |toolbar| 2 * toolbar.dpi / 96)
    }

    fn extend_stroke(&mut self, at: (i32, i32)) {
        let (Some(region), Some(stroke)) = (self.chosen, self.strokes.last_mut()) else { return };
        let from = stroke.last().copied().unwrap_or(at);
        stroke.push(at);
        let radius = self.radius();
        pencil_line(&mut self.pixels, self.width, self.height, region, from, at, radius);
    }

    fn undo(&mut self) {
        if self.strokes.pop().is_none() {
            return;
        }
        self.pixels.clone_from(&self.clean);
        let Some(region) = self.chosen else { return };
        let radius = self.radius();
        for stroke in &self.strokes {
            for pair in stroke.windows(2) {
                pencil_line(&mut self.pixels, self.width, self.height, region, pair[0], pair[1], radius);
            }
        }
    }

    /// Returns true when the overlay is finished.
    fn press(&mut self, at: (i32, i32)) -> bool {
        let Some(toolbar) = &self.toolbar else {
            self.start = Some(at);
            self.current = at;
            return false;
        };
        match toolbar.tool_at(at) {
            Some(Tool::Send) => {
                self.send = true;
                return true;
            }
            Some(Tool::Cancel) => return true,
            Some(Tool::Undo) => self.undo(),
            None if self.chosen.is_some_and(|region| inside(region, at)) => {
                self.drawing = true;
                self.strokes.push(vec![at]);
                self.extend_stroke(at);
            }
            None => {}
        }
        false
    }

    fn moved(&mut self, at: (i32, i32)) -> bool {
        if self.drawing {
            self.extend_stroke(at);
            return true;
        }
        if self.start.is_some() {
            self.current = at;
            return true;
        }
        let hover = self.toolbar.as_ref().and_then(|toolbar| toolbar.tool_at(at));
        let changed = hover != self.hover;
        self.hover = hover;
        changed
    }

    fn released(&mut self, at: (i32, i32)) {
        self.drawing = false;
        let Some(start) = self.start.take() else { return };
        let dragged = drag_rect(start, at);
        let big_enough = dragged.2 - dragged.0 >= SMALLEST_DRAG && dragged.3 - dragged.1 >= SMALLEST_DRAG;
        let whole = (0, 0, self.width, self.height);
        let monitor = monitor_rect_at(at, self.origin).unwrap_or(whole);
        self.choose(if big_enough { dragged } else { monitor }, monitor, dpi_at(at, self.origin));
    }
}

thread_local! {
    static SHOT: RefCell<Option<Shot>> = const { RefCell::new(None) };
}

fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
            biWidth: width,
            // Negative means top-down rows.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    }
}

pub(crate) fn grab_screen() -> Option<Shot> {
    // SAFETY: every GDI object taken here is released before returning.
    unsafe {
        let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (width, height) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let old = SelectObject(memory, bitmap.into());
        let copied = BitBlt(memory, 0, 0, width, height, Some(screen), x, y, SRCCOPY | CAPTUREBLT).is_ok();
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let mut info = bitmap_info(width, height);
        SelectObject(memory, old);
        let lines = GetDIBits(memory, bitmap, 0, height as u32, Some(pixels.as_mut_ptr().cast()), &raw mut info, DIB_RGB_COLORS);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        if !copied || lines == 0 {
            return None;
        }
        Some(Shot::new(pixels, width, height, (x, y)))
    }
}

fn blit(dc: HDC, pixels: &[u8], width: i32, height: i32, rect: (i32, i32, i32, i32)) {
    let info = bitmap_info(width, height);
    // SAFETY: `pixels` holds width*height BGRA pixels, matching `info`, and is read during the call.
    unsafe {
        SetDIBitsToDevice(dc, rect.0, rect.1, (rect.2 - rect.0) as u32, (rect.3 - rect.1) as u32, rect.0, height - rect.3, 0, height as u32, pixels.as_ptr().cast(), &raw const info, DIB_RGB_COLORS);
    }
}

unsafe fn round_fill(dc: HDC, area: Area, color: u32, corner: i32) {
    // SAFETY: the brush is released here, and the stock pen needs no release.
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        let (old_brush, old_pen) = (SelectObject(dc, brush.into()), SelectObject(dc, GetStockObject(NULL_PEN)));
        let _ = RoundRect(dc, area.0, area.1, area.2 + 1, area.3 + 1, corner, corner);
        SelectObject(dc, old_pen);
        SelectObject(dc, old_brush);
        let _ = DeleteObject(brush.into());
    }
}

unsafe fn label(dc: HDC, text: &str, area: Area, color: u32, align: DRAW_TEXT_FORMAT) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT { left: area.0, top: area.1, right: area.2, bottom: area.3 };
    // SAFETY: `wide` and `rect` outlive the calls on the caller's device context.
    unsafe {
        SetTextColor(dc, COLORREF(color));
        DrawTextW(dc, &mut wide, &raw mut rect, align | DT_SINGLELINE | DT_VCENTER);
    }
}

unsafe fn draw_toolbar(dc: HDC, toolbar: &Toolbar, hover: Option<Tool>, can_undo: bool) {
    let scale = |value: i32| value * toolbar.dpi / 96;
    // SAFETY: the font is released here, on the caller's device context.
    unsafe {
        round_fill(dc, toolbar.area, 0x0014_1414, scale(14));
        let font = CreateFontW(-scale(13), 0, 0, 0, 500, 0, 0, 0, FONT_CHARSET(1), FONT_OUTPUT_PRECISION(0), FONT_CLIP_PRECISION(0), FONT_QUALITY(5), 0, w!("Segoe UI"));
        let old_font = SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        label(dc, HINT, toolbar.hint, 0x00A0_A0A0, DT_LEFT);
        for &(tool, text, area) in &toolbar.buttons {
            let hovered = hover == Some(tool);
            if tool == Tool::Send {
                round_fill(dc, area, if hovered { 0x00D8_D8D8 } else { 0x00FF_FFFF }, scale(8));
                label(dc, text, area, 0x0000_0000, DT_CENTER);
            } else {
                if hovered && (tool != Tool::Undo || can_undo) {
                    round_fill(dc, area, 0x0030_3030, scale(8));
                }
                label(dc, text, area, if tool == Tool::Undo && !can_undo { 0x0060_6060 } else { 0x00FF_FFFF }, DT_CENTER);
            }
        }
        SelectObject(dc, old_font);
        let _ = DeleteObject(font.into());
    }
}

unsafe fn draw(dc: HDC, shot: &Shot) {
    blit(dc, &shot.dim, shot.width, shot.height, (0, 0, shot.width, shot.height));
    if let Some(rect) = shot.start.map(|start| drag_rect(start, shot.current)).or(shot.chosen) {
        blit(dc, &shot.pixels, shot.width, shot.height, rect);
        // SAFETY: the brush is created and released here.
        unsafe {
            let brush = CreateSolidBrush(COLORREF(0x00FF_FFFF));
            let frame = RECT { left: rect.0, top: rect.1, right: rect.2, bottom: rect.3 };
            FrameRect(dc, &raw const frame, brush);
            let _ = DeleteObject(brush.into());
        }
    }
    if let Some(toolbar) = &shot.toolbar {
        // SAFETY: `dc` is the caller's live device context.
        unsafe { draw_toolbar(dc, toolbar, shot.hover, !shot.strokes.is_empty()) };
    }
}

fn paint(window: HWND) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: called from WM_PAINT on this thread's window; GDI objects are released here.
    unsafe {
        let screen = BeginPaint(window, &raw mut paint);
        SHOT.with(|shot| {
            let shot = shot.borrow();
            let Some(shot) = shot.as_ref() else { return };
            let back = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, shot.width, shot.height);
            let old = SelectObject(back, bitmap.into());
            draw(back, shot);
            let _ = BitBlt(screen, 0, 0, shot.width, shot.height, Some(back), 0, 0, SRCCOPY);
            SelectObject(back, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(back);
        });
        let _ = EndPaint(window, &raw const paint);
    }
}

fn point_of(lparam: LPARAM) -> (i32, i32) {
    (i32::from((lparam.0 & 0xFFFF) as u16 as i16), i32::from(((lparam.0 >> 16) & 0xFFFF) as u16 as i16))
}

// A click without a drag takes the whole screen under the cursor, in window coordinates.
fn dpi_at(point: (i32, i32), origin: (i32, i32)) -> i32 {
    let (mut x, mut y) = (96u32, 96u32);
    // SAFETY: both out-pointers are valid; on failure the 96 defaults stand.
    let _ = unsafe { GetDpiForMonitor(MonitorFromPoint(POINT { x: point.0 + origin.0, y: point.1 + origin.1 }, MONITOR_DEFAULTTONEAREST), MDT_EFFECTIVE_DPI, &raw mut x, &raw mut y) };
    i32::try_from(x).unwrap_or(96)
}

fn monitor_rect_at(point: (i32, i32), origin: (i32, i32)) -> Option<Area> {
    let mut info = MONITORINFO { cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(0), ..MONITORINFO::default() };
    // SAFETY: `info` is a valid out-pointer with its size set.
    let found = unsafe { GetMonitorInfoW(MonitorFromPoint(POINT { x: point.0 + origin.0, y: point.1 + origin.1 }, MONITOR_DEFAULTTONEAREST), &raw mut info) }.as_bool();
    found.then_some((info.rcMonitor.left - origin.0, info.rcMonitor.top - origin.1, info.rcMonitor.right - origin.0, info.rcMonitor.bottom - origin.1))
}

fn with_shot<T: Default>(action: impl FnOnce(&mut Shot) -> T) -> T {
    SHOT.with(|shot| shot.borrow_mut().as_mut().map(action).unwrap_or_default())
}

fn repaint(window: HWND) {
    // SAFETY: invalidating our own window.
    let _ = unsafe { InvalidateRect(Some(window), None, false) };
}

fn close(window: HWND) {
    // SAFETY: destroying our own window, from its own thread.
    let _ = unsafe { DestroyWindow(window) };
}

unsafe extern "system" fn proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_LBUTTONDOWN => {
            if with_shot(|shot| shot.press(point_of(lparam))) {
                close(window);
            } else {
                repaint(window);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if with_shot(|shot| shot.moved(point_of(lparam))) {
                repaint(window);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            with_shot(|shot| shot.released(point_of(lparam)));
            repaint(window);
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            close(window);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == ESCAPE => {
            close(window);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == ENTER => {
            let chosen = with_shot(|shot| {
                shot.send = shot.chosen.is_some();
                shot.send
            });
            if chosen {
                close(window);
            }
            LRESULT(0)
        }
        // SAFETY: reading a key's state takes no pointers.
        WM_KEYDOWN if wparam.0 == LETTER_Z && unsafe { GetKeyState(i32::from(VK_CONTROL.0)) } < 0 => {
            with_shot(Shot::undo);
            repaint(window);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let mut at = POINT::default();
            // SAFETY: `at` is a valid out-pointer; a failed read just keeps the crosshair.
            let _ = unsafe { GetCursorPos(&raw mut at) };
            let over_bar = with_shot(|shot| shot.toolbar.as_ref().is_some_and(|toolbar| inside(toolbar.area, (at.x - shot.origin.0, at.y - shot.origin.1))));
            if over_bar {
                // SAFETY: loading and setting a stock cursor.
                let _ = unsafe { SetCursor(LoadCursorW(None, IDC_ARROW).ok()) };
                return LRESULT(1);
            }
            // SAFETY: Windows's default handling restores the crosshair.
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(window);
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ending this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: everything else gets Windows's default handling.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe extern "system" fn raise_if_ours(window: HWND, _: LPARAM) -> BOOL {
    // SAFETY: plain queries and activation on a window Windows just handed us.
    unsafe {
        let mut owner = 0u32;
        GetWindowThreadProcessId(window, Some(&raw mut owner));
        if owner != GetCurrentProcessId() || !IsWindowVisible(window).as_bool() || GetWindowTextLengthW(window) == 0 {
            return BOOL(1);
        }
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        }
        let _ = SetForegroundWindow(window);
    }
    BOOL(0)
}

fn raise_olympus() {
    // SAFETY: the callback only runs during this call.
    let _ = unsafe { EnumWindows(Some(raise_if_ours), LPARAM(0)) };
}

/// Freezes the screen, lets you pick a region and draw on it, and hands back the saved image; no message is sent.
pub fn select(on_done: impl FnOnce(PathBuf) + Send + 'static) {
    if OPEN.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        // SAFETY: the class and name are static, and this thread pumps messages below.
        let chosen = unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            grab_screen().and_then(|shot| {
                let (x, y, width, height) = (shot.origin.0, shot.origin.1, shot.width, shot.height);
                SHOT.with(|slot| *slot.borrow_mut() = Some(shot));
                let class = w!("OlympusShot");
                let cursor = LoadCursorW(None, IDC_CROSS).unwrap_or_default();
                let description = WNDCLASSW { lpfnWndProc: Some(proc), lpszClassName: class, hInstance: HINSTANCE::default(), hCursor: cursor, ..WNDCLASSW::default() };
                RegisterClassW(&raw const description);
                let style = WINDOW_EX_STYLE(WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0);
                let window = CreateWindowExW(style, class, PCWSTR::null(), WINDOW_STYLE(WS_POPUP.0), x, y, width, height, None, None, None, None).ok()?;
                let _ = ShowWindow(window, SW_SHOW);
                let _ = SetForegroundWindow(window);
                let mut message = MSG::default();
                while GetMessageW(&raw mut message, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
                SHOT.with(|slot| slot.borrow_mut().take())
            })
        };
        OPEN.store(false, Ordering::SeqCst);
        let Some(shot) = chosen else { return };
        let (true, Some(rect)) = (shot.send, shot.chosen) else { return };
        let (width, height, rgba) = crop_rgba(&shot.pixels, shot.width, shot.height, rect);
        let path = new_attachment_path("png");
        if save_png(&path, width, height, &rgba).is_ok() {
            raise_olympus();
            on_done(path);
        }
    });
}

/// Clears out old attachments in the background.
pub fn prune_in_background() {
    std::thread::spawn(|| prune_attachments(&attachments_dir(), KEEP_FOR));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_in_any_direction_makes_the_same_rectangle() {
        assert_eq!(drag_rect((10, 20), (30, 40)), (10, 20, 30, 40));
        assert_eq!(drag_rect((30, 40), (10, 20)), (10, 20, 30, 40));
        assert_eq!(drag_rect((30, 20), (10, 40)), (10, 20, 30, 40));
    }

    #[test]
    fn cropping_swaps_blue_and_red_and_takes_the_right_pixels() {
        // Two rows of two BGRA pixels; each pixel's blue value is its number.
        let pixels = [1, 0, 9, 0, 2, 0, 9, 0, 3, 0, 9, 0, 4, 0, 9, 0];
        let (width, height, rgba) = crop_rgba(&pixels, 2, 2, (1, 1, 2, 2));
        assert_eq!((width, height), (1, 1));
        assert_eq!(rgba, [9, 0, 4, 255]);
    }

    #[test]
    fn a_rectangle_past_the_screen_edge_is_clipped_not_a_crash() {
        let pixels = [0u8; 16];
        let (width, height, rgba) = crop_rgba(&pixels, 2, 2, (-5, -5, 50, 50));
        assert_eq!((width, height, rgba.len()), (2, 2, 16));
        let (width, height, rgba) = crop_rgba(&pixels, 2, 2, (9, 9, 9, 9));
        assert_eq!((width, height, rgba.len()), (0, 0, 0));
    }

    #[test]
    fn dimming_halves_colour_and_stays_opaque() {
        assert_eq!(dimmed(&[200, 100, 50, 0]), [100, 50, 25, 255]);
    }

    fn picked_shot() -> Shot {
        let mut shot = Shot::new(vec![10; 100 * 100 * 4], 100, 100, (0, 0));
        shot.choose((10, 10, 60, 60), (0, 0, 800, 600), 96);
        shot
    }

    fn is_pencil(shot: &Shot, x: i32, y: i32) -> bool {
        let start = ((y * shot.width + x) * 4) as usize;
        shot.pixels[start..start + 4] == PENCIL
    }

    fn tool_center(shot: &Shot, tool: Tool) -> (i32, i32) {
        let area = shot.toolbar.as_ref().expect("toolbar").buttons.iter().find(|button| button.0 == tool).expect("button").2;
        ((area.0 + area.2) / 2, (area.1 + area.3) / 2)
    }

    #[test]
    fn a_fast_stroke_is_a_connected_line_that_stays_inside_the_picture() {
        let mut shot = picked_shot();
        assert!(!shot.press((20, 20)));
        shot.moved((50, 20));
        shot.moved((90, 20));
        shot.released((90, 20));
        assert!((20..60).all(|x| is_pencil(&shot, x, 20)));
        assert!(!is_pencil(&shot, 60, 20) && !is_pencil(&shot, 70, 20));
    }

    #[test]
    fn a_single_click_leaves_a_dot_and_presses_outside_the_picture_draw_nothing() {
        let mut shot = picked_shot();
        shot.press((30, 30));
        shot.released((30, 30));
        assert!(is_pencil(&shot, 30, 30));
        shot.press((80, 80));
        assert_eq!(shot.strokes.len(), 1);
    }

    #[test]
    fn undo_takes_back_only_the_last_stroke_and_stops_at_the_clean_picture() {
        let mut shot = picked_shot();
        for y in [20, 40] {
            shot.press((20, y));
            shot.moved((30, y));
            shot.released((30, y));
        }
        shot.undo();
        assert!(is_pencil(&shot, 25, 20) && !is_pencil(&shot, 25, 40));
        shot.undo();
        shot.undo();
        assert!(shot.pixels == shot.clean);
    }

    #[test]
    fn only_the_send_button_sends() {
        let mut shot = picked_shot();
        assert!(shot.press(tool_center(&shot, Tool::Cancel)) && !shot.send);
        let mut shot = picked_shot();
        assert!(!shot.press(tool_center(&shot, Tool::Undo)) && !shot.send);
        assert!(shot.press(tool_center(&shot, Tool::Send)) && shot.send);
    }

    #[test]
    fn a_new_selection_is_not_finished_until_you_say_so() {
        let mut shot = Shot::new(vec![10; 100 * 100 * 4], 100, 100, (0, 0));
        assert!(!shot.press((10, 10)));
        shot.moved((50, 50));
        shot.released((50, 50));
        assert_eq!(shot.chosen, Some((10, 10, 50, 50)));
        assert!(!shot.send && shot.toolbar.is_some());
    }

    #[test]
    fn the_toolbar_stays_on_the_screen_wherever_the_picture_is() {
        let monitor = (0, 0, 1000, 800);
        let below = Toolbar::new((100, 100, 700, 300), monitor, 96);
        assert_eq!((below.area.1, below.area.2), (308, 700));
        let above = Toolbar::new((100, 500, 700, 790), monitor, 96);
        assert!(above.area.3 <= 500);
        let whole = Toolbar::new(monitor, monitor, 96);
        assert!(whole.area.3 <= 800 && whole.area.2 <= 1000);
        let corner = Toolbar::new((0, 0, 20, 20), monitor, 96);
        assert!(corner.area.0 >= 0);
        let second = Toolbar::new((1100, 100, 1120, 120), (1000, 0, 2000, 800), 96);
        assert!(second.area.0 >= 1000);
    }

    #[test]
    #[ignore]
    fn capture_the_drawing_overlay() {
        let out = std::env::var("OLYMPUS_PILL_DIR").expect("a folder for the pictures");
        let (width, height) = (1100, 700);
        let pixels: Vec<u8> = (0..height).flat_map(|y| (0..width).flat_map(move |x| [(x / 5) as u8, (y / 4) as u8, 90, 255])).collect();
        let mut shot = Shot::new(pixels, width, height, (0, 0));
        shot.choose((150, 90, 950, 560), (0, 0, width, height), 96);
        shot.press((250, 200));
        for at in [(350, 150), (500, 300), (650, 180), (800, 400)] {
            shot.moved(at);
        }
        shot.released((800, 400));
        shot.moved(tool_center(&shot, Tool::Undo));
        // SAFETY: GDI objects made here are released here.
        let rgba = unsafe {
            let screen = GetDC(None);
            let memory = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, width, height);
            let old = SelectObject(memory, bitmap.into());
            draw(memory, &shot);
            SelectObject(memory, old);
            let mut drawn = vec![0u8; (width * height * 4) as usize];
            let mut info = bitmap_info(width, height);
            GetDIBits(memory, bitmap, 0, height as u32, Some(drawn.as_mut_ptr().cast()), &raw mut info, DIB_RGB_COLORS);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);
            drawn
        };
        let (_, _, opaque) = crop_rgba(&rgba, width, height, (0, 0, width, height));
        save_png(&Path::new(&out).join("overlay.png"), width as u32, height as u32, &opaque).expect("saved");
    }

    #[test]
    fn saved_screenshots_are_real_pngs() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("nested/shot.png");
        save_png(&path, 1, 1, &[1, 2, 3, 255]).expect("saved");
        let bytes = std::fs::read(path).expect("read");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn only_old_attachments_are_removed() {
        let dir = tempfile::tempdir().expect("dir");
        let fresh = dir.path().join("fresh.png");
        std::fs::write(&fresh, "x").expect("write");
        prune_attachments(dir.path(), std::time::Duration::from_secs(3600));
        assert!(fresh.exists());
        prune_attachments(dir.path(), std::time::Duration::ZERO);
        std::thread::sleep(std::time::Duration::from_millis(20));
        prune_attachments(dir.path(), std::time::Duration::ZERO);
        assert!(!fresh.exists());
    }
}
