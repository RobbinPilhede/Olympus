mod chrome;
mod composer_view;
mod overlay;
pub mod parts;
mod review;
mod terminal;
mod transcript;

use crate::app::{start_ingest, Action, App};
use crate::commands;
use crate::input::{KeyCode, KeyEvent, KeyModifiers};
use crate::markdown::{Inline, Tone};
use crate::session::{Pane, Window as OWindow};
use crate::theme;
use gpui::prelude::*;
use gpui::{
    canvas, div, font, px, size, Animation, AnimationExt as _, AnyElement, App as GApp, Application, Bounds, ClipboardItem, Context, ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Font,
    FontStyle, FontWeight, Hsla, KeyDownEvent, ListAlignment, ListState, MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels, Point, SharedString, Stateful, Div, TextLayout, TextRun, TitlebarOptions, UTF16Selection, Window, WindowBounds,
    WindowOptions,
};
use olympus_core::config::home_dir;
use olympus_core::runtime::Runtime;
use olympus_core::target::Provider;
use parts::{Part, Selection};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use crate::app::AppEvent;
use crate::hotkeys::{Chord, Signal};
use gpui::ExternalPaths;
use tokio::sync::mpsc::unbounded_channel;

const FONT: &str = "JetBrains Mono";
const LOGO: &str = "olympus.svg";
const CLAUDE_MARK: &str = "claude.svg";
const OPENAI_MARK: &str = "openai.svg";
const LINE: f32 = 21.0;
const BODY: f32 = 13.5;

/// One piece of painted transcript text, for mapping the mouse to a character.
#[derive(Clone)]
struct PaintedText {
    window: u64,
    block: usize,
    part: usize,
    /// Where this text starts within its part; table cells are pieces of one part.
    base: usize,
    layout: TextLayout,
    bounds: Bounds<Pixels>,
}

/// A wheel or page scroll easing toward where it's headed.
struct Glide {
    position: f32,
    target: f32,
    last: Instant,
}

/// What the interface keeps per chat.
struct ChatView {
    list: ListState,
    count: usize,
    parts: Vec<Option<Rc<Vec<Part>>>>,
    born: HashMap<usize, Instant>,
    glide: Option<Glide>,
    expanded: HashSet<usize>,
}

/// A drag selection in a terminal, in screen cells.
#[derive(Clone, Copy)]
struct TermSelection {
    window: u64,
    start: (u16, u16),
    end: (u16, u16),
    dragging: bool,
}

/// The cell grid of a painted terminal.
#[derive(Clone, Copy)]
struct TermGrid {
    origin: Point<Pixels>,
    cell: gpui::Size<Pixels>,
}

/// The window's root view.
pub struct Root {
    app: App,
    focus: FocusHandle,
    views: HashMap<u64, ChatView>,
    painted: Rc<RefCell<Vec<PaintedText>>>,
    selection: Option<Selection>,
    term_selection: Option<TermSelection>,
    term_grids: HashMap<u64, TermGrid>,
    composer_layout: Option<TextLayout>,
    composer_dragging: bool,
    composer_scroll: gpui::ScrollHandle,
    threads_scroll: gpui::ScrollHandle,
    dragged_thread: Option<u64>,
    caret: Option<Bounds<Pixels>>,
    sidebar: bool,
    flash: Option<SharedString>,
    toaster: Option<crate::notify::Toaster>,
    toast_generation: u64,
    window_active: bool,
    review_scroll: gpui::UniformListScrollHandle,
}

struct Assets;

impl gpui::AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
            LOGO => Some(Cow::Borrowed(include_bytes!("../../assets/icon/olympus.svg").as_slice())),
            CLAUDE_MARK => Some(Cow::Borrowed(include_bytes!("../../assets/icon/claude.svg").as_slice())),
            OPENAI_MARK => Some(Cow::Borrowed(include_bytes!("../../assets/icon/openai.svg").as_slice())),
            _ => None,
        })
    }

    fn list(&self, _: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(vec![LOGO.into(), CLAUDE_MARK.into(), OPENAI_MARK.into()])
    }
}

/// The Olympus mark at a given size.
fn logo(size: f32) -> impl IntoElement {
    gpui::svg().path(LOGO).flex_none().size(px(size)).text_color(theme::text())
}

fn system_prefers_reduced_motion() -> bool {
    use windows::Win32::Foundation::TRUE;
    use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};
    let mut enabled = TRUE;
    // SAFETY: the pointer is to a live BOOL, which is what this setting writes.
    let read = unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some(std::ptr::from_mut(&mut enabled).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)) };
    read.is_ok() && !enabled.as_bool()
}

/// Opens the window and runs until the user quits.
pub fn run(rt: Arc<Runtime>, cwd: PathBuf) {
    Application::new().with_assets(Assets).run(move |cx: &mut GApp| {
        let fonts: Vec<Cow<'static, [u8]>> = vec![
            Cow::Borrowed(include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf")),
            Cow::Borrowed(include_bytes!("../../assets/fonts/JetBrainsMono-Medium.ttf")),
            Cow::Borrowed(include_bytes!("../../assets/fonts/JetBrainsMono-Bold.ttf")),
            Cow::Borrowed(include_bytes!("../../assets/fonts/JetBrainsMono-Italic.ttf")),
        ];
        cx.text_system().add_fonts(fonts).expect("the bundled fonts are valid");
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions { title: Some("Olympus".into()), appears_transparent: true, traffic_light_position: None }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1320.), px(840.)), cx))),
            window_min_size: Some(size(px(560.), px(380.))),
            app_id: Some("olympus".into()),
            ..WindowOptions::default()
        };
        cx.open_window(options, |window, cx| cx.new(|cx| Root::new(rt, cwd, window, cx))).expect("opening the window");
        cx.activate(true);
    });
}

impl Root {
    fn new(rt: Arc<Runtime>, cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = unbounded_channel();
        let mut app = App::new(rt, tx.clone(), home_dir().join("sessions"), cwd);
        app.reduce_motion |= system_prefers_reduced_motion();
        app.refresh_account_labels();
        crate::capture::prune_in_background();
        crate::hotkeys::set_enabled(!home_dir().join(crate::app::HOTKEYS_OFF).exists());
        let (voice, shots) = (crate::voice::spawn(), tx.clone());
        crate::hotkeys::spawn(move |signal| match signal {
            Signal::Start(Chord::Voice) => voice.start(false),
            Signal::StartLocked => voice.start(true),
            Signal::Stop(Chord::Voice) => voice.stop(),
            Signal::Start(Chord::Shot) => {
                let shots = shots.clone();
                crate::capture::select(move |path| {
                    let _ = shots.send(AppEvent::Attach(path));
                });
            }
            Signal::Stop(Chord::Shot) => {}
        });
        start_ingest(tx);
        cx.spawn(async move |this, cx| {
            while let Some(event) = rx.recv().await {
                let mut batch = vec![event];
                while let Ok(next) = rx.try_recv() {
                    batch.push(next);
                }
                let updated = this.update(cx, |root, cx| {
                    for event in batch {
                        root.app.handle(event);
                    }
                    root.after_change(cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
        let root = cx.entity();
        window.on_window_should_close(cx, move |_, cx| {
            let root = root.read(cx);
            root.app.save_all();
            if let Some(toaster) = &root.toaster {
                toaster.hide();
            }
            // Background work such as a history import would otherwise hold the process open.
            std::process::exit(0);
        });
        let focus = cx.focus_handle();
        focus.focus(window);
        Self {
            app,
            focus,
            views: HashMap::new(),
            painted: Rc::default(),
            selection: None,
            term_selection: None,
            term_grids: HashMap::new(),
            composer_layout: None,
            composer_dragging: false,
            composer_scroll: gpui::ScrollHandle::new(),
            threads_scroll: gpui::ScrollHandle::new(),
            dragged_thread: None,
            caret: None,
            sidebar: true,
            flash: None,
            toaster: crate::notify::Toaster::new(),
            toast_generation: 0,
            window_active: true,
            review_scroll: gpui::UniformListScrollHandle::new(),
        }
    }

    fn after_change(&mut self, cx: &mut Context<Self>) {
        if self.app.quit {
            self.app.save_all();
            if let Some(toaster) = &self.toaster {
                toaster.hide();
            }
            std::process::exit(0);
        }
        self.announce(cx);
        if let Some(text) = self.app.pending_flash.take() {
            self.flash_for(&text, Duration::from_secs(5), cx);
        }
        if let Some(text) = self.app.pending_copy.take() {
            self.copy(text, cx);
        }
        if std::mem::take(&mut self.app.pending_details) {
            self.toggle_last_details();
        }
        if let Some((provider, account)) = self.app.pending_folder.take() {
            self.browse_for_folder(provider, account, cx);
        }
        cx.notify();
    }

    // Cancelling the dialog goes back to the folder list, so the chosen account isn't lost.
    fn browse_for_folder(&mut self, provider: Provider, account: Option<PathBuf>, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Start here".into()) });
        cx.spawn(async move |this, cx| {
            let folder = chosen.await.ok().and_then(Result::ok).flatten().and_then(|paths| paths.into_iter().next());
            let action = match folder {
                Some(cwd) => Action::NewChatIn { provider, account, cwd },
                None => Action::NewChatOn(provider, account),
            };
            let _ = this.update(cx, |root, cx| {
                commands::pick(&mut root.app, action);
                root.after_change(cx);
            });
        })
        .detach();
    }

    // The sound always plays; the banner only when you aren't already looking at that thread.
    fn announce(&mut self, cx: &mut Context<Self>) {
        let notices = std::mem::take(&mut self.app.notices);
        let Some(last) = notices.last() else { return };
        crate::notify::play(last.ok);
        let unseen = notices.iter().rev().find(|notice| !self.window_active || notice.window != self.app.active_id());
        let (Some(notice), Some(toaster)) = (unseen, &self.toaster) else { return };
        toaster.show(&notice.title, &notice.body);
        self.toast_generation += 1;
        let generation = self.toast_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(12)).await;
            let _ = this.update(cx, |root, _| {
                if root.toast_generation == generation {
                    if let Some(toaster) = &root.toaster {
                        toaster.hide();
                    }
                }
            });
        })
        .detach();
    }

    // Text wins, because spreadsheets also put a picture of the cells on the clipboard.
    fn paste_item(&mut self, item: &gpui::ClipboardItem) {
        if let Some(text) = item.text() {
            self.app.paste(&text);
            return;
        }
        for entry in item.entries() {
            match entry {
                gpui::ClipboardEntry::Image(image) => {
                    let extension = match image.format {
                        gpui::ImageFormat::Jpeg => "jpg",
                        gpui::ImageFormat::Gif => "gif",
                        gpui::ImageFormat::Webp => "webp",
                        _ => "png",
                    };
                    let path = crate::capture::new_attachment_path(extension);
                    let saved = path.parent().is_some_and(|folder| std::fs::create_dir_all(folder).is_ok()) && std::fs::write(&path, image.bytes()).is_ok();
                    if saved {
                        self.app.attach(path);
                    }
                }
                gpui::ClipboardEntry::String(_) => {}
            }
        }
    }

    fn run_command(&mut self, line: &str, cx: &mut Context<Self>) {
        commands::run(&mut self.app, line);
        self.after_change(cx);
    }

    fn flash(&mut self, text: &str, cx: &mut Context<Self>) {
        self.flash_for(text, Duration::from_millis(1400), cx);
    }

    fn flash_for(&mut self, text: &str, duration: Duration, cx: &mut Context<Self>) {
        self.flash = Some(SharedString::from(text.to_string()));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(duration).await;
            let _ = this.update(cx, |root, cx| {
                root.flash = None;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn copy(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.flash("Copied", cx);
    }

    fn toggle_last_details(&mut self) {
        let id = self.app.active_id();
        let Some(Pane::Chat(chat)) = self.app.windows.get(self.app.active).map(|w| &w.pane) else { return };
        let found = chat.state.blocks.iter().rposition(|block| parts::block_parts(block).iter().any(|part| matches!(&part.kind, parts::PartKind::Tool { detail, .. } if !detail.is_empty())));
        let (Some(block), Some(view)) = (found, self.views.get_mut(&id)) else { return };
        if !view.expanded.remove(&block) {
            view.expanded.insert(block);
        }
        view.list.splice(block..block + 1, 1);
    }

    fn selection_text(&self) -> Option<String> {
        let selection = self.selection.filter(Selection::has_text)?;
        let window = self.app.windows.iter().find(|w| w.id == selection.window)?;
        match &window.pane {
            Pane::Chat(chat) => Some(selection.text(&chat.state.blocks)),
            Pane::Term(_) => None,
        }
    }

    fn term_selection_text(&self) -> Option<String> {
        let selection = self.term_selection.filter(|s| s.start != s.end)?;
        let window = self.app.windows.iter().find(|w| w.id == selection.window)?;
        let Pane::Term(term) = &window.pane else { return None };
        let parser = term.pty.parser.lock().ok()?;
        let (a, b) = (selection.start.min(selection.end), selection.start.max(selection.end));
        Some(parser.screen().contents_between(a.0, a.1, b.0, b.1))
    }

    fn sync_views(&mut self, window: &mut Window) {
        let live: HashSet<u64> = self.app.windows.iter().map(|w| w.id).collect();
        self.views.retain(|id, _| live.contains(id));
        let reduce_motion = self.app.reduce_motion;
        let pending = std::mem::take(&mut self.app.pending_scroll);
        let active = self.app.active_id();
        let mut moving = false;
        for app_window in &mut self.app.windows {
            let id = app_window.id;
            let Some(chat) = OWindow::chat(app_window) else { continue };
            let view = self.views.entry(id).or_insert_with(|| ChatView {
                list: ListState::new(0, ListAlignment::Bottom, px(900.)),
                count: 0,
                parts: Vec::new(),
                born: HashMap::new(),
                glide: None,
                expanded: HashSet::new(),
            });
            let fresh = view.count == 0;
            if let Some(from) = chat.dirty_from.take() {
                let new_count = chat.state.blocks.len();
                let from = from.min(view.count).min(new_count);
                let top = view.list.logical_scroll_top();
                let following = top.item_ix >= view.count;
                view.list.splice(from..view.count, new_count - from);
                if !following && top.item_ix < new_count {
                    view.list.scroll_to(top);
                }
                view.parts.truncate(from);
                view.parts.resize(new_count, None);
                if !fresh && !reduce_motion {
                    let now = Instant::now();
                    view.born.retain(|_, at| at.elapsed() < Duration::from_millis(400));
                    for index in view.count..new_count {
                        view.born.insert(index, now);
                    }
                }
                view.count = new_count;
            }
            if std::mem::take(&mut chat.follow) {
                view.glide = None;
                let bottom = view.list.max_offset_for_scrollbar().height;
                view.list.set_offset_from_scrollbar(gpui::point(px(0.), bottom));
            }
            if id == active && pending != 0 {
                view.scroll_by(pending as f32 * LINE, reduce_motion);
            }
            moving |= view.advance_glide();
        }
        if moving {
            window.request_animation_frame();
        }
    }

    fn wheel(&mut self, id: u64, delta: f32) {
        let reduce_motion = self.app.reduce_motion;
        if let Some(view) = self.views.get_mut(&id) {
            view.scroll_by(delta, reduce_motion);
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        let m = &stroke.modifiers;
        // AltGr arrives as Ctrl+Alt; on many layouts it's how you type @, $, { and [.
        if m.control && m.alt && stroke.key_char.is_some() {
            return;
        }
        let modifiers = KeyModifiers { ctrl: m.control, alt: m.alt, shift: m.shift };
        let in_term = !self.app.active_is_chat();
        match (stroke.key.as_str(), m.control, m.alt, m.shift) {
            ("b", false, true, false) => {
                self.sidebar = !self.sidebar;
                cx.stop_propagation();
                cx.notify();
                return;
            }
            ("pageup" | "pagedown", false, true, false) if self.app.diff_open => {
                let direction = if stroke.key == "pageup" { 1.0 } else { -1.0 };
                let handle = self.review_scroll.0.borrow().base_handle.clone();
                let offset = handle.offset();
                handle.set_offset(gpui::point(offset.x, (offset.y + px(direction * 300.0)).min(px(0.))));
                cx.stop_propagation();
                cx.notify();
                return;
            }
            ("c", true, false, shift) if !in_term || shift => {
                let text = if in_term { self.term_selection_text() } else { self.selection_text().or_else(|| self.app.composer.selected_text().map(String::from)) };
                if let Some(text) = text {
                    self.selection = None;
                    self.term_selection = None;
                    self.copy(text, cx);
                    cx.stop_propagation();
                    return;
                }
                if shift {
                    cx.stop_propagation();
                    return;
                }
            }
            ("x", true, false, false) if !in_term => {
                if let Some(text) = self.app.composer.selected_text().map(String::from) {
                    self.app.composer.delete_selection();
                    self.copy(text, cx);
                    cx.stop_propagation();
                    return;
                }
            }
            ("v", true, false, _) | ("insert", false, false, true) => {
                let files = crate::capture::clipboard_files();
                if !files.is_empty() && !in_term {
                    for file in files {
                        self.app.attach(file);
                    }
                } else if let Some(item) = cx.read_from_clipboard() {
                    self.paste_item(&item);
                }
                cx.stop_propagation();
                self.after_change(cx);
                return;
            }
            _ => {}
        }
        let Some(code) = key_code(&stroke.key, m.control || m.alt) else { return };
        let code = if code == KeyCode::Tab && m.shift { KeyCode::BackTab } else { code };
        self.selection = None;
        self.app.key(KeyEvent::new(code, modifiers));
        cx.stop_propagation();
        self.after_change(cx);
        window.invalidate_character_coordinates();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer_dragging {
            self.composer_drag(event.position, cx);
        }
        self.selection_drag(event.position, window, cx);
        self.term_drag(event.position, cx);
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.composer_dragging = false;
        if let Some(selection) = &mut self.selection {
            selection.dragging = false;
        }
        if let Some(selection) = &mut self.term_selection {
            selection.dragging = false;
        }
        cx.notify();
    }
}

impl ChatView {
    fn scroll_by(&mut self, delta: f32, reduce_motion: bool) {
        let max = f32::from(self.list.max_offset_for_scrollbar().height);
        let at_tail = self.list.logical_scroll_top().item_ix >= self.count;
        let current = if at_tail { max } else { -f32::from(self.list.scroll_px_offset_for_scrollbar().y) };
        let glide = self.glide.get_or_insert(Glide { position: current, target: current, last: Instant::now() });
        glide.target = (glide.target - delta).clamp(0.0, max);
        if reduce_motion {
            glide.position = glide.target;
        }
    }

    fn advance_glide(&mut self) -> bool {
        let Some(glide) = &mut self.glide else { return false };
        let now = Instant::now();
        let step = now.duration_since(glide.last).as_secs_f32().min(0.05);
        glide.last = now;
        glide.position += (glide.target - glide.position) * (1.0 - (-step * 18.0).exp());
        let settled = (glide.target - glide.position).abs() < 0.5;
        if settled {
            glide.position = glide.target;
        }
        self.list.set_offset_from_scrollbar(gpui::point(px(0.), px(glide.position)));
        if settled {
            self.glide = None;
        }
        !settled
    }
}

fn key_code(key: &str, with_modifier: bool) -> Option<KeyCode> {
    Some(match key {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "escape" => KeyCode::Esc,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "space" if with_modifier => KeyCode::Char(' '),
        other => {
            if let Some(number) = other.strip_prefix('f').and_then(|n| n.parse().ok()) {
                return Some(KeyCode::F(number));
            }
            let mut chars = other.chars();
            let c = chars.next()?;
            // Plain characters arrive through the input handler, which also covers dead keys and IMEs.
            if !with_modifier || chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    })
}

fn tone_color(tone: Tone) -> Hsla {
    match tone {
        Tone::Text => theme::text(),
        Tone::Dim => theme::dim(),
        Tone::Accent => theme::accent(),
        Tone::Red => theme::red(),
        Tone::Bright => theme::bright(),
    }
}

fn inline_font(style: &Inline) -> Font {
    let mut base = font(FONT);
    if style.bold {
        base.weight = FontWeight::BOLD;
    }
    if style.italic {
        base.style = FontStyle::Italic;
    }
    base
}

fn text_run(len: usize, style: &Inline, selected: bool) -> TextRun {
    let background = if selected {
        Some(theme::tint(theme::accent(), 0.3))
    } else if style.code {
        Some(theme::raised())
    } else {
        None
    };
    TextRun {
        len,
        font: inline_font(style),
        color: tone_color(style.tone),
        background_color: background,
        underline: style.link.then(|| gpui::UnderlineStyle { thickness: px(1.), color: None, wavy: false }),
        strikethrough: style.strike.then(|| gpui::StrikethroughStyle { thickness: px(1.), color: None }),
    }
}

/// A small control with its hover and press styles; callers add its click handler.
fn control(id: impl Into<ElementId>, label: impl Into<SharedString>, color: Hsla) -> Stateful<Div> {
    control_base(id, label, color).hover(|s| s.bg(theme::overlay())).active(|s| s.bg(theme::line()))
}

/// The one filled control in a view: its primary action.
fn primary_control(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    control_base(id, label, theme::bg()).bg(theme::accent()).font_weight(FontWeight::BOLD).hover(|s| s.opacity(0.88)).active(|s| s.opacity(0.76))
}

fn control_base(id: impl Into<ElementId>, label: impl Into<SharedString>, color: Hsla) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(24.))
        .px(px(8.))
        .rounded(px(6.))
        .text_size(px(12.))
        .text_color(color)
        .whitespace_nowrap()
        .cursor_pointer()
        .child(label.into())
}

/// A pulsing mark for work in progress; a steady one when motion is reduced.
fn live_mark(id: impl Into<ElementId>, reduce_motion: bool) -> AnyElement {
    let mark = div().flex_none().text_color(theme::accent()).child("●");
    if reduce_motion {
        return mark.into_any_element();
    }
    mark.with_animation(id, Animation::new(Duration::from_millis(1100)).repeat().with_easing(gpui::pulsating_between(0.35, 1.0)), |el, delta| el.opacity(delta)).into_any_element()
}

fn utf16_to_utf8(text: &str, target: usize) -> usize {
    let (mut utf8, mut utf16) = (0, 0);
    for c in text.chars() {
        if utf16 >= target {
            break;
        }
        utf16 += c.len_utf16();
        utf8 += c.len_utf8();
    }
    utf8
}

fn utf8_to_utf16(text: &str, target: usize) -> usize {
    text[..target.min(text.len())].encode_utf16().count()
}

impl Root {
    fn composer_takes_text(&self) -> bool {
        self.app.picker.is_none() && self.app.active_is_chat()
    }
}

impl EntityInputHandler for Root {
    fn text_for_range(&mut self, range: Range<usize>, actual: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        if !self.composer_takes_text() {
            return None;
        }
        let text = &self.app.composer.text;
        let bytes = utf16_to_utf8(text, range.start)..utf16_to_utf8(text, range.end);
        actual.replace(utf8_to_utf16(text, bytes.start)..utf8_to_utf16(text, bytes.end));
        text.get(bytes).map(String::from)
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        if !self.composer_takes_text() {
            return Some(UTF16Selection { range: 0..0, reversed: false });
        }
        let composer = &self.app.composer;
        let range = composer.selection().unwrap_or(composer.cursor..composer.cursor);
        Some(UTF16Selection { range: utf8_to_utf16(&composer.text, range.start)..utf8_to_utf16(&composer.text, range.end), reversed: composer.anchor.is_some_and(|a| a > composer.cursor) })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let composer = &self.app.composer;
        composer.marked.as_ref().map(|range| utf8_to_utf16(&composer.text, range.start)..utf8_to_utf16(&composer.text, range.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.app.composer.marked = None;
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        self.selection = None;
        if self.composer_takes_text() {
            let composer = &mut self.app.composer;
            let target = range.map(|r| utf16_to_utf8(&composer.text, r.start)..utf16_to_utf8(&composer.text, r.end)).or_else(|| composer.marked.take());
            if let Some(target) = target {
                composer.anchor = Some(target.start);
                composer.cursor = target.end;
            }
            composer.marked = None;
        }
        self.app.text_input(text);
        self.after_change(cx);
    }

    fn replace_and_mark_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, _: Option<Range<usize>>, _: &mut Window, cx: &mut Context<Self>) {
        if !self.composer_takes_text() {
            return;
        }
        let composer = &mut self.app.composer;
        let target = range.map(|r| utf16_to_utf8(&composer.text, r.start)..utf16_to_utf8(&composer.text, r.end)).or_else(|| composer.marked.take());
        if let Some(target) = target {
            composer.anchor = Some(target.start);
            composer.cursor = target.end;
        }
        composer.delete_selection();
        let start = composer.cursor;
        composer.insert(text);
        composer.marked = (!text.is_empty()).then_some(start..start + text.len());
        cx.notify();
    }

    fn bounds_for_range(&mut self, _: Range<usize>, _: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        self.caret
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let layout = self.composer_layout.as_ref()?;
        let index = layout.index_for_position(point).unwrap_or_else(|nearest| nearest);
        Some(utf8_to_utf16(&self.app.composer.text, index))
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.painted.borrow_mut().clear();
        self.window_active = window.is_window_active();
        self.sync_views(window);
        let width = f32::from(window.viewport_size().width);
        let entity: Entity<Root> = cx.entity();
        let focus = self.focus.clone();
        let show_review = self.app.diff_open && self.app.active_is_chat() && !self.app.grid;
        let show_sidebar = self.sidebar && width >= 760.0 && !(show_review && width < 1120.0);
        let review_replaces_main = show_review && width < 900.0;
        div()
            .id("root")
            .size_full()
            .flex()
            .flex_col()
            .relative()
            .bg(theme::bg())
            .font_family(FONT)
            .text_size(px(BODY))
            .line_height(px(LINE))
            .text_color(theme::text())
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::on_mouse_up))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(theme::raised()))
            .on_drop(cx.listener(|root, paths: &ExternalPaths, _, cx| {
                for path in paths.paths() {
                    root.app.attach(path.clone());
                }
                root.after_change(cx);
            }))
            .child(canvas(|_, _, _| (), move |bounds, _, window, cx| window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx)).absolute().top_0().left_0().size_full())
            .child(self.titlebar(width, cx))
            // Focus tracking swallows mouse presses, so it must not cover the title bar or Windows can't drag it.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .when(show_sidebar, |el| el.child(self.sidebar_view(cx)))
                            .when(!review_replaces_main, |el| el.child(self.main_view(cx)))
                            .when(show_review, |el| el.child(self.review_panel(review_replaces_main, cx))),
                    )
                    .child(self.footer(width, !show_sidebar)),
            )
            .when(self.app.picker.is_some(), |el| el.child(self.picker_overlay(f32::from(window.viewport_size().height), cx)))
    }
}

impl Root {
    fn main_view(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.app.grid { self.grid_view(cx) } else { self.window_view(self.app.active, cx) };
        div().flex().flex_col().flex_1().min_w_0().min_h_0().child(div().flex_1().min_h_0().child(content)).when(self.app.active_is_chat(), |el| el.child(self.composer_view(cx)))
    }

    fn window_view(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(window) = self.app.windows.get(index) else { return div().into_any_element() };
        let id = window.id;
        if matches!(window.pane, Pane::Chat(_)) {
            self.chat_body(id, cx)
        } else {
            self.term_view(id, cx)
        }
    }

    fn grid_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.app.windows.len().min(4);
        let start = self.app.active.saturating_sub(3);
        let tile = |root: &mut Self, index: usize, cx: &mut Context<Self>| {
            let focused = index == root.app.active;
            let title: SharedString = root.app.windows[index].title().to_string().into();
            let number = index + 1;
            let body = root.window_view(index, cx);
            div()
                .id(("tile", index))
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .rounded(px(10.))
                .border_1()
                .border_color(if focused { theme::accent() } else { theme::line() })
                .bg(theme::bg())
                .overflow_hidden()
                .on_click(cx.listener(move |root, _, _, cx| {
                    root.app.focus(index);
                    cx.notify();
                }))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .gap_2()
                        .h(px(28.))
                        .items_center()
                        .px(px(10.))
                        .bg(theme::surface())
                        .text_size(px(12.))
                        .text_color(if focused { theme::accent() } else { theme::dim() })
                        .child(number.to_string())
                        .child(div().line_clamp(1).text_ellipsis().child(title)),
                )
                .child(div().flex_1().min_h_0().child(body))
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut index = start;
        let per_row = if count > 2 { 2 } else { count.max(1) };
        while index < start + count {
            let end = (index + per_row).min(start + count);
            let mut row = div().flex().flex_1().min_h_0().gap_2();
            for i in index..end {
                row = row.child(tile(self, i, cx));
            }
            rows.push(row.into_any_element());
            index = end;
        }
        div().size_full().flex().flex_col().gap_2().p_2().children(rows).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_and_plain_characters_are_left_for_the_input_handler() {
        assert_eq!(key_code("enter", false), Some(KeyCode::Enter));
        assert_eq!(key_code("f5", false), Some(KeyCode::F(5)));
        assert_eq!(key_code("a", false), None);
        assert_eq!(key_code("a", true), Some(KeyCode::Char('a')));
        assert_eq!(key_code("space", false), None);
        assert_eq!(key_code("space", true), Some(KeyCode::Char(' ')));
    }

    #[test]
    fn utf16_offsets_convert_both_ways_across_multibyte_text() {
        let text = "aå😀b";
        assert_eq!(utf8_to_utf16(text, text.len()), 5);
        assert_eq!(utf16_to_utf8(text, 2), 3);
        assert_eq!(utf16_to_utf8(text, 4), 7);
        assert_eq!(utf8_to_utf16(text, 3), 2);
    }
}
