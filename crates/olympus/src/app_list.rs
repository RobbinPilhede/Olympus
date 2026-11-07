//! The apps you can choose between on Obiview's Apps page: names and icons from Windows itself.
use olympus_obi::capture::{classify, is_noise_process, never_read, normalize_exe, open_apps, plain_name};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, MAX_PATH};
use windows::Win32::Graphics::Gdi::{DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ};
use windows::Win32::UI::Shell::{IShellLinkW, SHGetFileInfoW, ShellLink, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

const NOT_APPS: [&str; 12] = ["uninstall", "readme", "read me", "help", "manual", "documentation", "license", "website", "setup", "installer", "repair", "release notes"];
const NOT_APP_EXES: [&str; 5] = ["msiexec", "rundll32", "hh", "explorer", "unins000"];

/// One app on the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppEntry {
    pub exe: String,
    pub name: String,
    pub open: bool,
    pub locked: bool,
    pub icon: Option<PathBuf>,
}

/// Which program a shortcut really starts, seeing through the updater some apps launch themselves with.
pub fn shortcut_exe(target: &str, arguments: &str) -> Option<String> {
    let mut words = arguments.split_whitespace();
    if let Some(started) = words.by_ref().skip_while(|word| !word.eq_ignore_ascii_case("--processStart")).nth(1) {
        return Some(normalize_exe(started.trim_matches('"')));
    }
    target.to_lowercase().ends_with(".exe").then(|| normalize_exe(target))
}

/// False for shortcuts to uninstallers, manuals and the like.
pub fn looks_like_an_app(name: &str, exe: &str) -> bool {
    let name = name.to_lowercase();
    !exe.is_empty() && !exe.starts_with("olympus") && !NOT_APPS.iter().any(|word| name.contains(word)) && !NOT_APP_EXES.contains(&exe) && !is_noise_process(exe)
}

/// Open apps first, then by name.
pub fn sorted(mut apps: Vec<AppEntry>) -> Vec<AppEntry> {
    apps.sort_by_key(|app| (!app.open, app.name.to_lowercase()));
    apps
}

fn wide_text(buffer: &[u16]) -> String {
    String::from_utf16_lossy(&buffer[..buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len())])
}

// The program itself gives a cleaner icon than a shortcut, which comes with an arrow on it.
fn read_shortcut(path: &Path) -> Option<(String, PathBuf)> {
    // SAFETY: COM is started on this thread by `scan`, and the buffers outlive the calls.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        link.cast::<IPersistFile>().ok()?.Load(&HSTRING::from(path.as_os_str()), STGM_READ).ok()?;
        let mut target = [0u16; MAX_PATH as usize];
        link.GetPath(&mut target, std::ptr::null_mut(), 0).ok()?;
        let mut arguments = [0u16; 1024];
        let _ = link.GetArguments(&mut arguments);
        let target = wide_text(&target);
        let exe = shortcut_exe(&target, &wide_text(&arguments))?;
        let icon = if normalize_exe(&target) == exe { PathBuf::from(target) } else { path.to_path_buf() };
        Some((exe, icon))
    }
}

fn shortcuts_in(folder: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            shortcuts_in(&path, found);
        } else if path.extension().is_some_and(|ending| ending.eq_ignore_ascii_case("lnk")) {
            found.push(path);
        }
    }
}

// What the program calls itself, from the version details inside the file.
fn description_of(path: &str) -> Option<String> {
    let name = HSTRING::from(path);
    // SAFETY: `block` is as long as Windows asked for, and the returned pointers point into it.
    unsafe {
        let size = GetFileVersionInfoSizeW(&name, None);
        let mut block = vec![0u8; size as usize];
        GetFileVersionInfoW(&name, None, size, block.as_mut_ptr().cast()).ok()?;
        let (mut language, mut length) = (std::ptr::null_mut(), 0u32);
        VerQueryValueW(block.as_ptr().cast(), &HSTRING::from(r"\VarFileInfo\Translation"), &raw mut language, &raw mut length).as_bool().then_some(())?;
        let codes = std::slice::from_raw_parts(language.cast::<u16>(), 2);
        let key = format!(r"\StringFileInfo\{:04x}{:04x}\FileDescription", codes[0], codes[1]);
        let (mut text, mut units) = (std::ptr::null_mut(), 0u32);
        VerQueryValueW(block.as_ptr().cast(), &HSTRING::from(key), &raw mut text, &raw mut units).as_bool().then_some(())?;
        let text = wide_text(std::slice::from_raw_parts(text.cast::<u16>(), units as usize));
        (!text.trim().is_empty()).then(|| text.trim().to_string())
    }
}

/// The icon Windows shows for a file or shortcut, as a PNG.
pub fn icon_png(path: &Path) -> Option<Vec<u8>> {
    let mut info = SHFILEINFOW::default();
    // SAFETY: the name and `info` outlive the call, and the size given is `info`'s.
    let got = unsafe { SHGetFileInfoW(PCWSTR(HSTRING::from(path.as_os_str()).as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(0), Some(&raw mut info), size_of::<SHFILEINFOW>() as u32, SHGFI_ICON | SHGFI_LARGEICON) };
    if got == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let mut parts = ICONINFO::default();
    // SAFETY: the icon is valid here, and its bitmaps and handle are freed below on every path.
    let pixels = unsafe {
        let pixels = GetIconInfo(info.hIcon, &raw mut parts).ok().and_then(|()| {
            let mut header = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: 32, biHeight: -32, biPlanes: 1, biBitCount: 32, ..Default::default() }, ..Default::default() };
            let mut pixels = vec![0u8; 32 * 32 * 4];
            let screen = GetDC(Some(HWND::default()));
            let lines = GetDIBits(screen, parts.hbmColor, 0, 32, Some(pixels.as_mut_ptr().cast()), &raw mut header, DIB_RGB_COLORS);
            ReleaseDC(Some(HWND::default()), screen);
            (lines == 32).then_some(pixels)
        });
        let _ = DeleteObject(parts.hbmColor.into());
        let _ = DeleteObject(parts.hbmMask.into());
        let _ = DestroyIcon(info.hIcon);
        pixels
    }?;
    let opaque = pixels.chunks_exact(4).all(|pixel| pixel[3] == 0);
    let mut rgba = Vec::with_capacity(pixels.len());
    for pixel in pixels.chunks_exact(4) {
        // Old icons have no alpha at all, and would come out invisible.
        rgba.extend([pixel[2], pixel[1], pixel[0], if opaque { 255 } else { pixel[3] }]);
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, 32, 32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().ok()?.write_image_data(&rgba).ok()?;
    Some(out)
}

fn cached_icon(folder: &Path, exe: &str, source: Option<&Path>) -> Option<PathBuf> {
    let file = folder.join(format!("{}.png", exe.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_")));
    if file.exists() {
        return Some(file);
    }
    let png = icon_png(source?)?;
    std::fs::create_dir_all(folder).ok()?;
    std::fs::write(&file, png).ok()?;
    Some(file)
}

/// Finds every app: from the Start menu, what's open now, and what Obiview has seen before.
pub fn scan(icons: &Path, seen: &[String], only_pids: &[u32]) -> Vec<AppEntry> {
    // SAFETY: paired with the uninitialize at the end, on this thread.
    let started = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let mut apps: BTreeMap<String, (String, bool, Option<PathBuf>)> = BTreeMap::new();
    if only_pids.is_empty() {
        let mut shortcuts = Vec::new();
        for base in ["ProgramData", "APPDATA"] {
            if let Some(base) = std::env::var_os(base) {
                shortcuts_in(&PathBuf::from(base).join(r"Microsoft\Windows\Start Menu\Programs"), &mut shortcuts);
            }
        }
        for shortcut in shortcuts {
            let name = shortcut.file_stem().map_or_else(String::new, |stem| stem.to_string_lossy().to_string());
            if let Some((exe, icon)) = read_shortcut(&shortcut).filter(|(exe, _)| looks_like_an_app(&name, exe)) {
                apps.entry(exe).or_insert((name, false, Some(icon)));
            }
        }
    }
    for (exe, path) in open_apps(only_pids) {
        let friendly = || plain_name(&exe).map(str::to_string).or_else(|| classify(&exe, "", None).1).or_else(|| description_of(&path)).unwrap_or_else(|| exe.clone());
        if looks_like_an_app(&exe, &exe) {
            let entry = apps.entry(exe.clone()).or_insert_with(|| (friendly(), false, Some(PathBuf::from(&path))));
            entry.1 = true;
            entry.2 = Some(PathBuf::from(&path));
        }
    }
    for exe in seen {
        if looks_like_an_app(exe, exe) {
            apps.entry(exe.clone()).or_insert_with(|| (plain_name(exe).map(str::to_string).or_else(|| classify(exe, "", None).1).unwrap_or_else(|| exe.clone()), false, None));
        }
    }
    let list = apps.into_iter().map(|(exe, (name, open, source))| AppEntry { icon: cached_icon(icons, &exe, source.as_deref()), locked: never_read(&exe), exe, name, open }).collect();
    if started {
        // SAFETY: this thread started COM above and no longer uses it.
        unsafe { CoUninitialize() };
    }
    sorted(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, open: bool) -> AppEntry {
        AppEntry { exe: name.to_lowercase(), name: name.into(), open, locked: false, icon: None }
    }

    #[test]
    fn a_shortcut_means_the_program_it_ends_up_starting() {
        assert_eq!(shortcut_exe(r"C:\Apps\Discord\Update.exe", r#"--processStart "Discord.exe" --flag"#).as_deref(), Some("discord"));
        assert_eq!(shortcut_exe(r"C:\Program Files\Editor\Code.exe", "").as_deref(), Some("code"));
        assert_eq!(shortcut_exe(r"C:\Docs\manual.pdf", ""), None);
    }

    #[test]
    fn installers_and_helpers_are_not_offered_as_apps() {
        assert!(looks_like_an_app("Discord", "discord"));
        assert!(!looks_like_an_app("Uninstall Discord", "update"));
        assert!(!looks_like_an_app("Olympus", "olympus"));
        assert!(!looks_like_an_app("Run", "rundll32"));
    }

    #[test]
    fn open_apps_come_first_then_by_name() {
        let names: Vec<String> = sorted(vec![entry("Zed", true), entry("Word", false), entry("Arc", true), entry("adobe", false)]).into_iter().map(|app| app.name).collect();
        assert_eq!(names, ["Arc", "Zed", "adobe", "Word"]);
    }

    #[test]
    fn a_real_program_gives_a_real_icon_and_name_and_the_list_is_cached() {
        let windows = std::env::var("WINDIR").expect("WINDIR");
        let notepad = Path::new(&windows).join("explorer.exe");
        let png = icon_png(&notepad).expect("an icon");
        assert!(png.starts_with(b"\x89PNG"), "a PNG");
        assert!(description_of(&notepad.to_string_lossy()).is_some(), "Windows' own programs say what they are");
        let dir = tempfile::tempdir().expect("dir");
        let first = cached_icon(dir.path(), "explorer", Some(&notepad)).expect("saved");
        assert_eq!(cached_icon(dir.path(), "explorer", None), Some(first), "already saved, so the source isn't needed");
    }
}
