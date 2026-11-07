use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW, InvalidateRect, MonitorFromWindow, SelectObject, SetBkMode, SetTextColor, SetWindowRgn, BitBlt, DT_LEFT, DT_SINGLELINE, DT_VCENTER, FONT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY, HBRUSH, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT, SRCCOPY, TRANSPARENT};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetMessageW, PostMessageW, RegisterClassW, SetTimer, SetWindowPos, ShowWindow, HMENU, HWND_TOPMOST, MSG, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WINDOW_STYLE, WM_APP, WM_ERASEBKGND, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP};

const SHOW: u32 = WM_APP + 1;
const HIDE: u32 = WM_APP + 2;
const HIDE_LATER: usize = 2;
const REPAINT: usize = 1;
const BARS: usize = 5;

static WINDOW: AtomicIsize = AtomicIsize::new(0);
static LEVEL: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Debug, PartialEq)]
enum Look {
    Listening,
    /// Listening with nothing held down, so it says how to stop.
    Locked,
    Text(String),
}

static LOOK: Mutex<Option<Look>> = Mutex::new(None);

/// The small status pill shown while voice is working; it never takes focus from what you're typing in.
#[derive(Clone)]
pub struct Pill {
    _keep: Arc<()>,
}

fn post(message: u32, wparam: usize) {
    let window = HWND(WINDOW.load(Ordering::Relaxed) as *mut _);
    if !window.is_invalid() {
        // SAFETY: posting to a window that may be gone just fails.
        let _ = unsafe { PostMessageW(Some(window), message, WPARAM(wparam), LPARAM(0)) };
    }
}

fn set_look(look: Look, hide_after: bool) {
    if let Ok(mut slot) = LOOK.lock() {
        *slot = Some(look);
    }
    post(SHOW, usize::from(hide_after));
}

impl Pill {
    /// Shows the listening bars.
    pub fn show_listening(&self, locked: bool) {
        LEVEL.store(0, Ordering::Relaxed);
        set_look(if locked { Look::Locked } else { Look::Listening }, false);
    }

    /// Shows that speech is being turned into text.
    pub fn show_working(&self) {
        set_look(Look::Text("Transcribing".into()), false);
    }

    /// Shows that the model is being fetched for the first time.
    pub fn show_downloading(&self) {
        set_look(Look::Text("Downloading voice model".into()), false);
    }

    /// Shows a short message, then goes away by itself.
    pub fn show_message(&self, text: &str) {
        set_look(Look::Text(text.to_string()), true);
    }

    /// Shows a status that stays until something replaces it.
    pub fn show_status(&self, text: &str) {
        set_look(Look::Text(text.to_string()), false);
    }

    /// Hides the pill.
    pub fn hide(&self) {
        post(HIDE, 0);
    }

    /// Feeds the bars from the microphone.
    pub fn set_level(&self, rms: f32) {
        LEVEL.store(rms.to_bits(), Ordering::Relaxed);
    }
}

/// How tall each bar is, 0 to 1, for a microphone level; the wave shape keeps it alive at low volume.
pub fn bar_heights(rms: f32, phase: u32) -> [f32; BARS] {
    let loudness = (rms * 14.0).clamp(0.0, 1.0).sqrt();
    let mut bars = [0.0; BARS];
    for (index, bar) in bars.iter_mut().enumerate() {
        let wave = ((phase as f32 * 0.55) + index as f32 * 1.3).sin() * 0.5 + 0.5;
        *bar = (0.16 + loudness * (0.45 + 0.55 * wave)).min(1.0);
    }
    bars
}

fn fill(dc: windows::Win32::Graphics::Gdi::HDC, rect: RECT, color: u32) {
    // SAFETY: the brush is created, used and deleted here, on a device context owned by the caller.
    unsafe {
        let brush: HBRUSH = CreateSolidBrush(COLORREF(color));
        FillRect(dc, &raw const rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

fn dpi_of(window: HWND) -> i32 {
    let (mut x, mut y) = (96u32, 96u32);
    // SAFETY: both out-pointers are valid; on failure the 96 defaults stand.
    let _ = unsafe { GetDpiForMonitor(MonitorFromWindow(window, MONITOR_DEFAULTTOPRIMARY), MDT_EFFECTIVE_DPI, &raw mut x, &raw mut y) };
    i32::try_from(x).unwrap_or(96)
}

unsafe fn paint(window: HWND, phase: u32) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: called from WM_PAINT with this thread's window; every GDI object made here is released here.
    unsafe {
        let screen = BeginPaint(window, &raw mut paint);
        let dpi = dpi_of(window);
        let (width, height) = (scaled_size(dpi).0, scaled_size(dpi).1);
        let back = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let old = SelectObject(back, bitmap.into());
        fill(back, RECT { left: 0, top: 0, right: width, bottom: height }, 0x0000_0000);
        let look = LOOK.lock().ok().and_then(|slot| slot.clone()).unwrap_or(Look::Listening);
        if matches!(look, Look::Listening | Look::Locked) {
            let rms = f32::from_bits(LEVEL.load(Ordering::Relaxed));
            let (bar, gap) = (3 * dpi / 96, 4 * dpi / 96);
            let total = bar * BARS as i32 + gap * (BARS as i32 - 1);
            let room = height - 14 * dpi / 96;
            let start = if look == Look::Locked { 16 * dpi / 96 } else { (width - total) / 2 };
            for (index, level) in bar_heights(rms, phase).iter().enumerate() {
                let tall = ((room as f32 * level) as i32).max(bar);
                let left = start + index as i32 * (bar + gap);
                let top = (height - tall) / 2;
                fill(back, RECT { left, top, right: left + bar, bottom: top + tall }, 0x00FF_FFFF);
            }
        }
        let label = match &look {
            Look::Listening => None,
            Look::Locked => Some(("Shift+V to stop", 66)),
            Look::Text(text) => Some((text.as_str(), 16)),
        };
        if let Some((text, indent)) = label {
            {
                let font = CreateFontW(-13 * dpi / 96, 0, 0, 0, 500, 0, 0, 0, FONT_CHARSET(1), FONT_OUTPUT_PRECISION(0), FONT_CLIP_PRECISION(0), FONT_QUALITY(5), 0, w!("Segoe UI"));
                let old_font = SelectObject(back, font.into());
                SetBkMode(back, TRANSPARENT);
                SetTextColor(back, COLORREF(0x00FF_FFFF));
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                let mut area = RECT { left: indent * dpi / 96, top: 0, right: width - 8 * dpi / 96, bottom: height };
                DrawTextW(back, &mut wide, &raw mut area, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
                SelectObject(back, old_font);
                let _ = DeleteObject(font.into());
            }
        }
        let _ = BitBlt(screen, 0, 0, width, height, Some(back), 0, 0, SRCCOPY);
        SelectObject(back, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(back);
        let _ = EndPaint(window, &raw const paint);
    }
}

fn scaled_size(dpi: i32) -> (i32, i32) {
    let width = match LOOK.lock().ok().and_then(|slot| slot.clone()) {
        Some(Look::Locked) => 180,
        Some(Look::Text(text)) if text.chars().count() > 12 => 250,
        _ => 120,
    };
    (width * dpi / 96, 34 * dpi / 96)
}

// Above the taskbar of the screen you're working on, where it won't cover what you're typing in.
fn place(window: HWND) {
    // SAFETY: plain window queries and moves on our own window; a failed query keeps it where it was.
    unsafe {
        let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO { cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(0), ..MONITORINFO::default() };
        if !GetMonitorInfoW(monitor, &raw mut info).as_bool() {
            return;
        }
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi_x, &raw mut dpi_y);
        let dpi = i32::try_from(dpi_x).unwrap_or(96);
        let (width, height) = scaled_size(dpi);
        let area = info.rcWork;
        let left = area.left + (area.right - area.left - width) / 2;
        let top = area.bottom - height - 20 * dpi / 96;
        let _ = SetWindowPos(window, Some(HWND_TOPMOST), left, top, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, height, height);
        let _ = SetWindowRgn(window, Some(region), true);
    }
}

unsafe extern "system" fn proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    static PHASE: AtomicU32 = AtomicU32::new(0);
    match message {
        SHOW => {
            place(window);
            // SAFETY: both timers belong to this window, on this thread.
            unsafe {
                SetTimer(Some(window), REPAINT, 50, None);
                if wparam.0 != 0 {
                    SetTimer(Some(window), HIDE_LATER, 2500, None);
                }
                let _ = InvalidateRect(Some(window), None, false);
            }
            LRESULT(0)
        }
        HIDE => {
            // SAFETY: hiding our own window.
            let _ = unsafe { ShowWindow(window, SW_HIDE) };
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == HIDE_LATER => {
            // SAFETY: hiding our own window.
            let _ = unsafe { ShowWindow(window, SW_HIDE) };
            LRESULT(0)
        }
        WM_TIMER => {
            PHASE.fetch_add(1, Ordering::Relaxed);
            // SAFETY: invalidating our own window.
            let _ = unsafe { InvalidateRect(Some(window), None, false) };
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            // SAFETY: this is the WM_PAINT that `paint` expects.
            unsafe { paint(window, PHASE.load(Ordering::Relaxed)) };
            LRESULT(0)
        }
        // SAFETY: everything else gets Windows's default handling.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// Starts the pill's window on its own thread; it stays hidden until needed.
pub fn spawn() -> Pill {
    std::thread::spawn(|| {
        // SAFETY: the window class and name are static, and this thread pumps messages below.
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let class = w!("OlympusPill");
            let description = WNDCLASSW { lpfnWndProc: Some(proc), lpszClassName: class, hInstance: HINSTANCE::default(), ..WNDCLASSW::default() };
            RegisterClassW(&raw const description);
            let style = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED;
            let Ok(window) = CreateWindowExW(style, class, PCWSTR::null(), WINDOW_STYLE(WS_POPUP.0), 0, 0, 1, 1, None, None::<HMENU>, None, None) else { return };
            let _ = windows::Win32::UI::WindowsAndMessaging::SetLayeredWindowAttributes(window, COLORREF(0), 245, windows::Win32::UI::WindowsAndMessaging::LWA_ALPHA);
            WINDOW.store(window.0 as isize, Ordering::Relaxed);
            let mut message = MSG::default();
            while GetMessageW(&raw mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&raw const message);
            }
        }
    });
    Pill { _keep: Arc::new(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    #[test]
    #[ignore]
    fn capture_every_look_of_the_pill() {
        let out = std::env::var("OLYMPUS_PILL_DIR").expect("a folder for the pictures");
        // SAFETY: only sets this test thread's own DPI handling.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let pill = spawn();
        std::thread::sleep(std::time::Duration::from_millis(300));
        for (name, show) in [("listening", 0), ("locked", 1), ("working", 2), ("message", 3)] {
            match show {
                0 => pill.show_listening(false),
                1 => pill.show_listening(true),
                2 => pill.show_working(),
                _ => pill.show_message("No microphone found"),
            }
            pill.set_level(0.08);
            std::thread::sleep(std::time::Duration::from_millis(350));
            let mut rect = RECT::default();
            // SAFETY: the window is ours and `rect` is a valid out-pointer.
            unsafe { GetWindowRect(HWND(WINDOW.load(Ordering::Relaxed) as *mut _), &raw mut rect) }.expect("rect");
            let shot = crate::capture::grab_screen().expect("screen");
            let margin = 12;
            let area = (rect.left - shot.origin.0 - margin, rect.top - shot.origin.1 - margin, rect.right - shot.origin.0 + margin, rect.bottom - shot.origin.1 + margin);
            let (width, height, rgba) = crate::capture::crop_rgba(&shot.pixels, shot.width, shot.height, area);
            crate::capture::save_png(&std::path::Path::new(&out).join(format!("{name}.png")), width, height, &rgba).expect("saved");
        }
        pill.hide();
    }

    #[test]
    fn bars_rest_low_in_silence_and_rise_with_your_voice() {
        let quiet = bar_heights(0.0, 3);
        let loud = bar_heights(0.2, 3);
        assert!(quiet.iter().all(|bar| *bar < 0.2));
        assert!(loud.iter().sum::<f32>() > quiet.iter().sum::<f32>() * 3.0);
        assert!(loud.iter().all(|bar| *bar <= 1.0));
    }
}
