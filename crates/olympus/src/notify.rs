use std::cell::Cell;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIIF_LARGE_ICON, NIIF_NOSOUND, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, LoadIconW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE};

static DONE: &[u8] = include_bytes!("../assets/sounds/done.wav");
static FAILED: &[u8] = include_bytes!("../assets/sounds/failed.wav");
static LISTEN: &[u8] = include_bytes!("../assets/sounds/listen.wav");
static LISTENED: &[u8] = include_bytes!("../assets/sounds/listened.wav");

/// Plays the finished or failed chime without waiting for it.
pub fn play(ok: bool) {
    let sound = if ok { DONE } else { FAILED };
    // SAFETY: the sound is static, so it outlives the asynchronous playback SND_MEMORY reads from.
    let _ = unsafe { PlaySoundW(PCWSTR(sound.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT) };
}

/// Plays the short tick for voice recording starting or stopping.
pub fn tick(starting: bool) {
    let sound = if starting { LISTEN } else { LISTENED };
    // SAFETY: the sound is static, so it outlives the asynchronous playback SND_MEMORY reads from.
    let _ = unsafe { PlaySoundW(PCWSTR(sound.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT) };
}

/// Copies text into a fixed Windows buffer, keeping it terminated.
pub fn fill(buffer: &mut [u16], text: &str) {
    let room = buffer.len().saturating_sub(1);
    let mut used = 0;
    for unit in text.encode_utf16().take(room) {
        buffer[used] = unit;
        used += 1;
    }
    if used < buffer.len() {
        buffer[used] = 0;
    }
}

/// Shows Windows notifications through a tray entry, which needs no installer or app registration.
pub struct Toaster {
    window: HWND,
    shown: Cell<bool>,
}

impl Toaster {
    /// Creates the invisible window the notifications belong to.
    pub fn new() -> Option<Self> {
        // SAFETY: the class and name are static strings, and a message-only window needs no other handles.
        let window = unsafe { CreateWindowExW(WINDOW_EX_STYLE(0), w!("STATIC"), w!("Olympus"), WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, None, None) }.ok()?;
        Some(Self { window, shown: Cell::new(false) })
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW { cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(0), hWnd: self.window, uID: 1, ..NOTIFYICONDATAW::default() }
    }

    /// Shows a notification; Olympus plays its own sound, so this one stays silent.
    pub fn show(&self, title: &str, body: &str) {
        let mut data = self.data();
        data.uFlags = NIF_ICON | NIF_INFO;
        // SAFETY: icon 1 is embedded in this exe by build.rs, and a failed load only means no icon.
        let icon = unsafe { GetModuleHandleW(None).ok().and_then(|module| LoadIconW(Some(module.into()), PCWSTR(1 as _)).ok()) };
        if let Some(icon) = icon {
            data.hIcon = icon;
            data.hBalloonIcon = icon;
        }
        data.dwInfoFlags = NIIF_USER | NIIF_NOSOUND | NIIF_LARGE_ICON;
        fill(&mut data.szInfoTitle, title);
        fill(&mut data.szInfo, body);
        let message = if self.shown.get() { NIM_MODIFY } else { NIM_ADD };
        // SAFETY: `data` is fully initialized and only read during the call.
        if unsafe { Shell_NotifyIconW(message, &raw const data) }.as_bool() {
            self.shown.set(true);
        }
    }

    /// Removes the tray entry, which would otherwise linger after exit until hovered.
    pub fn hide(&self) {
        if self.shown.replace(false) {
            let data = self.data();
            // SAFETY: `data` only identifies the entry and is read during the call.
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const data) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_seconds(bytes: &[u8]) -> f32 {
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        (bytes.len() - 44) as f32 / (rate as f32 * 2.0)
    }

    #[test]
    fn both_sounds_are_real_short_and_different() {
        assert!((1.0..2.0).contains(&wav_seconds(DONE)));
        assert!((0.8..2.0).contains(&wav_seconds(FAILED)));
        assert_ne!(DONE, FAILED);
        assert!(wav_seconds(LISTEN) < 0.2 && wav_seconds(LISTENED) < 0.2, "ticks must not get in the way");
    }

    #[test]
    fn notification_text_is_cut_to_fit_and_stays_terminated() {
        let mut small = [7u16; 4];
        fill(&mut small, "abcdef");
        assert_eq!(small, [97, 98, 99, 0]);
        let mut roomy = [7u16; 8];
        fill(&mut roomy, "åb");
        assert_eq!(&roomy[..3], &[229, 98, 0]);
    }
}
