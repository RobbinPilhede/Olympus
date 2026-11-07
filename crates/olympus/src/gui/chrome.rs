use super::{control, icon_control, live_mark, logo, Root, CLAUDE_MARK, HISTORY_ICON, NETWORK_ICON, OPENAI_MARK, SEARCH_ICON, TERMINAL_ICON, WRENCH_ICON};
use crate::session::{account_name, Activity, Choice, Pane, Window as OWindow};
use crate::theme;
use gpui::prelude::*;
use gpui::{div, point, px, AnimationExt as _, AnyElement, App, Context, DragMoveEvent, FontWeight, IntoElement, Pixels, Render, SharedString, Window, WindowControlArea};
use olympus_core::config::user_home;
use olympus_core::limits::{summarize, window_word, UsageSummary};
use olympus_core::target::Provider;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A path with the home folder shortened to `~`.
pub fn short_path(path: &Path) -> String {
    match path.strip_prefix(user_home()) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~\\{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The end of a path, which names the project, cut to fit `max` characters.
pub fn path_tail(path: &str, max: usize) -> String {
    let count = path.chars().count();
    if count <= max {
        return path.to_string();
    }
    let keep = max.saturating_sub(1);
    let tail: String = path.chars().skip(count - keep).collect();
    format!("…{tail}")
}

/// Tokens in a compact form.
pub fn format_tokens(value: u64) -> String {
    match value {
        v if v >= 1_000_000 => format!("{:.1}M", v as f64 / 1_000_000.0),
        v if v >= 1_000 => format!("{:.1}k", v as f64 / 1_000.0),
        v => v.to_string(),
    }
}

/// Which provider mark a thread shows, if it has one.
pub fn provider_mark(provider: Option<&Provider>) -> Option<&'static str> {
    match provider {
        Some(Provider::Claude) => Some(CLAUDE_MARK),
        Some(Provider::Codex) => Some(OPENAI_MARK),
        _ => None,
    }
}

/// The color of a provider mark: Claude's orange, ChatGPT's white.
pub fn mark_color(path: &str) -> gpui::Hsla {
    if path == CLAUDE_MARK {
        theme::claude()
    } else {
        theme::accent()
    }
}

/// Terminal windows running a real CLI get its mark too.
pub fn terminal_mark(title: &str) -> Option<&'static str> {
    if title.starts_with("Claude") || title.starts_with("Sign in") {
        Some(CLAUDE_MARK)
    } else if title.starts_with("Codex") {
        Some(OPENAI_MARK)
    } else {
        None
    }
}

/// What a thread's card shows under its topic.
#[derive(Debug, PartialEq)]
pub struct ThreadCard {
    pub who: String,
    pub vendor: Option<String>,
}

/// Names who a chat is connected to, and which quota it draws on.
pub fn thread_card(provider: Option<&Provider>, account: Option<&Path>, labels: &HashMap<PathBuf, String>, home: &Path) -> ThreadCard {
    match provider {
        Some(signed_in @ (Provider::Claude | Provider::Codex)) => {
            let (word, vendor, folder) = if *signed_in == Provider::Codex { ("ChatGPT", "codex", ".codex") } else { ("Claude", "claude", ".claude") };
            let name = account_name(account);
            let default_dir = home.join(folder);
            let who = match (labels.get(account.unwrap_or(&default_dir)), account) {
                (Some(label), None) => label.clone(),
                (Some(label), Some(_)) => format!("{name} · {label}"),
                (None, None) => word.to_string(),
                (None, Some(_)) => format!("{word} {name}"),
            };
            ThreadCard { who, vendor: Some(if account.is_some() { format!("{vendor}:{name}") } else { vendor.to_string() }) }
        }
        Some(Provider::OpenRouter) => ThreadCard { who: "OpenRouter".to_string(), vendor: None },
        Some(Provider::Node(name)) => ThreadCard { who: name.clone(), vendor: None },
        None => ThreadCard { who: "Chooses per message".to_string(), vendor: None },
    }
}

/// The model shown for a chat, including what auto chose.
pub fn model_label(choice: &Choice, last_target: Option<&olympus_core::target::Target>) -> String {
    match (choice, last_target) {
        (Choice::Auto, Some(target)) => format!("auto · {target}"),
        _ => choice.label(),
    }
}

/// What a thread's card shows, for the sidebar and the phone page alike.
pub struct ThreadInfo {
    pub who: String,
    pub details: Vec<String>,
    pub used: Option<f32>,
    pub alert: bool,
    pub mark: Option<&'static str>,
    pub has_accounts: bool,
}

/// Works out a thread's card.
pub fn thread_info(app: &crate::app::App, index: usize) -> ThreadInfo {
    let window = &app.windows[index];
    let limits = app.rt.limits.snapshot();
    let (who, usage, model, has_accounts, mark): (String, Option<UsageSummary>, String, bool, Option<&'static str>) = match &window.pane {
        Pane::Chat(chat) if crate::obiview::is_obiview(chat) => (app.obi.status_line(), None, String::new(), false, Some(super::JARVIS_MARK)),
        Pane::Chat(chat) => {
            let provider = chat.state.provider();
            let card = thread_card(provider, provider.and_then(|p| chat.state.account_on(p)), &app.account_labels, &user_home());
            let usage = card.vendor.as_deref().and_then(|vendor| summarize(&limits, vendor));
            let has_accounts = !matches!(provider, Some(Provider::Node(_)));
            let likely = if provider.is_none() { app.rt.likely_target().map(|target| target.provider) } else { None };
            (card.who, usage, model_label(&chat.state.choice, chat.state.last_target.as_ref()), has_accounts, provider_mark(provider.or(likely.as_ref())))
        }
        Pane::Term(term) => ("Terminal".to_string(), None, String::new(), false, terminal_mark(&term.title)),
    };
    let state_word = match window.activity {
        Activity::Running => "working",
        Activity::Unseen => "finished",
        Activity::Failed => "failed",
        Activity::Idle => "",
    };
    let alert = usage.as_ref().is_some_and(|u| u.alert);
    let used = usage.as_ref().map(|u| u.used);
    let looping = match &window.pane {
        Pane::Chat(chat) => match chat.running_elsewhere() {
            Some(device) => format!("running on {device}"),
            None => chat.looping.as_ref().map_or_else(String::new, crate::session::Loop::label),
        },
        Pane::Term(_) => String::new(),
    };
    let details = [state_word.to_string(), looping, usage.map_or_else(String::new, |u| u.detail), model].into_iter().filter(|part| !part.is_empty()).collect();
    ThreadInfo { who, details, used, alert, mark, has_accounts }
}

fn caption_button(id: &'static str, glyph: &'static str, area: WindowControlArea, danger: bool) -> impl IntoElement {
    let hover = if danger { theme::red() } else { theme::raised() };
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(46.))
        .h_full()
        .text_color(theme::dim())
        .window_control_area(area)
        .hover(move |s| s.bg(hover).text_color(if danger { theme::bg() } else { theme::text() }))
        .child(glyph)
}

const DRAG_SCROLL_EDGE: f32 = 28.;
const DRAG_SCROLL_STEP: f32 = 12.;

/// The thread being dragged, by its window id, since its place changes during the drag.
struct ThreadDrag(u64);

struct ThreadGhost(SharedString);

impl Render for ThreadGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(240.))
            .px(px(32.))
            .py(px(7.))
            .rounded(px(7.))
            .bg(theme::raised())
            .border_1()
            .border_color(theme::tint(theme::accent(), 0.45))
            .shadow_lg()
            .font_family(super::FONT)
            .line_height(px(18.))
            .text_size(px(12.5))
            .text_color(theme::text())
            .child(div().w_full().line_clamp(1).text_ellipsis().child(self.0.clone()))
    }
}

// Past the middle rather than on entering, so rows of different heights can't swap back and forth.
fn passes_row(from: usize, row: usize, y: Pixels, top: Pixels, bottom: Pixels) -> bool {
    let middle = (top + bottom) / 2.;
    (from < row && y > middle && y < bottom) || (from > row && y < middle && y > top)
}

impl Root {
    pub(super) fn titlebar(&mut self, width: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let window = self.app.windows.get(self.app.active);
        let place: SharedString = match window.map(|w| &w.pane) {
            Some(Pane::Chat(chat)) => path_tail(&short_path(&chat.state.cwd), ((width - 420.0) / 7.3).max(8.0) as usize).into(),
            Some(Pane::Term(term)) => term.title.clone().into(),
            None => SharedString::default(),
        };
        let position = (width < 760.0 || !self.sidebar).then(|| format!("{}/{}", self.app.active + 1, self.app.windows.len()));
        let review_open = self.app.diff_open;
        let grid_open = self.app.grid;
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(38.))
            .pl(px(14.))
            .bg(theme::surface())
            .border_b_1()
            .border_color(theme::line())
            .child(
                div()
                    .id("drag")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .pr(px(12.))
                    .items_center()
                    .gap(px(12.))
                    .window_control_area(WindowControlArea::Drag)
                    .child(div().flex().flex_none().items_center().gap(px(8.)).child(logo(17.)).child(div().font_weight(FontWeight::BOLD).text_color(theme::text()).text_size(px(13.)).child("OLYMPUS")))
                    .when_some(position, |el, position| el.child(div().flex_none().text_size(px(12.)).text_color(theme::accent()).child(position)))
                    .child(div().flex_1().min_w_0().line_clamp(1).text_ellipsis().text_size(px(12.)).text_color(theme::dim()).child(place)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .pr(px(8.))
                    .when(self.app.active_is_chat(), |el| {
                        el.child(control("toggle-review", if review_open { "Hide changes" } else { "Changes" }, if review_open { theme::accent() } else { theme::dim() }).on_click(cx.listener(|root, _, _, cx| {
                            root.app.toggle_diff();
                            cx.notify();
                        })))
                    })
                    .when(self.app.windows.len() > 1, |el| {
                        el.child(control("toggle-grid", if grid_open { "Single" } else { "Grid" }, if grid_open { theme::accent() } else { theme::dim() }).on_click(cx.listener(|root, _, _, cx| {
                            root.app.grid = !root.app.grid;
                            cx.notify();
                        })))
                    }),
            )
            .child(caption_button("minimize", "–", WindowControlArea::Min, false))
            .child(caption_button("maximize", "□", WindowControlArea::Max, false))
            .child(caption_button("close", "×", WindowControlArea::Close, true))
    }

    // Marks keep their brand color, so finished and failed show as a dot on the corner.
    fn thread_mark(&self, index: usize, activity: Activity, mark: Option<&'static str>) -> AnyElement {
        let status = match activity {
            Activity::Unseen => Some(theme::green()),
            Activity::Failed => Some(theme::red()),
            Activity::Running | Activity::Idle => None,
        };
        let Some(path) = mark else {
            return match activity {
                Activity::Running => live_mark(("activity", index), self.app.reduce_motion),
                Activity::Unseen => div().flex_none().text_color(theme::green()).child("●").into_any_element(),
                Activity::Failed => div().flex_none().text_color(theme::red()).child("✗").into_any_element(),
                Activity::Idle => div().flex_none().text_color(theme::dim()).child("·").into_any_element(),
            };
        };
        let icon = div()
            .relative()
            .flex_none()
            .mt(px(2.))
            .size(px(14.))
            .child(gpui::svg().path(path).size(px(14.)).text_color(mark_color(path)))
            .when_some(status, |el, color| el.child(div().absolute().right(px(-3.)).bottom(px(-3.)).size(px(8.)).rounded_full().bg(color).border_1().border_color(theme::surface())));
        if activity == Activity::Running && !self.app.reduce_motion {
            return icon.with_animation(("activity", index), gpui::Animation::new(std::time::Duration::from_millis(1100)).repeat().with_easing(gpui::pulsating_between(0.35, 1.0)), |el, delta| el.opacity(delta)).into_any_element();
        }
        icon.into_any_element()
    }

    fn thread_row(&self, index: usize, window: &OWindow, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = index == self.app.active;
        let title: SharedString = window.title().to_string().into();
        let ThreadInfo { who, details, used, alert, mark, has_accounts } = thread_info(&self.app, index);
        let group = SharedString::from(format!("thread-{index}"));
        let lifted = self.dragged_thread.filter(|_| cx.has_active_drag()) == Some(window.id);
        let ghost_title = title.clone();
        div()
            .id(("thread", index))
            .group(group.clone())
            .when(lifted, |el| el.opacity(0.35))
            .on_drag(ThreadDrag(window.id), move |_, _, _, cx: &mut App| cx.new(|_| ThreadGhost(ghost_title.clone())))
            .on_drag_move(cx.listener(move |root, event: &DragMoveEvent<ThreadDrag>, _, cx| root.drag_thread_over(index, event, cx)))
            .flex()
            .flex_none()
            .gap(px(8.))
            .px(px(8.))
            .py(px(7.))
            .rounded(px(7.))
            .cursor_pointer()
            .border_1()
            .border_color(if focused { theme::tint(theme::accent(), 0.45) } else { theme::tint(theme::accent(), 0.0) })
            .when(focused, |el| el.bg(theme::raised()))
            .when(!focused, |el| el.hover(|s| s.bg(theme::raised())))
            .on_click(cx.listener(move |root, _, _, cx| {
                root.app.focus(index);
                cx.notify();
            }))
            .child(div().flex_none().w(px(16.)).child(self.thread_mark(index, window.activity, mark)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().w_full().line_clamp(1).text_ellipsis().line_height(px(18.)).text_size(px(12.5)).text_color(theme::text()).when(focused, |el| el.font_weight(FontWeight::MEDIUM)).child(title))
                    .child(
                        div()
                            .id(("thread-account", index))
                            .flex()
                            .gap(px(4.))
                            .w_full()
                            .line_height(px(16.))
                            .text_size(px(12.))
                            .text_color(theme::dim())
                            .when(has_accounts, |el| {
                                el.hover(|s| s.text_color(theme::text())).on_click(cx.listener(move |root, _, _, cx| {
                                    cx.stop_propagation();
                                    root.app.focus(index);
                                    root.run_command("/account", cx);
                                }))
                            })
                            .child(div().min_w_0().line_clamp(1).text_ellipsis().child(SharedString::from(who)))
                            .when(has_accounts, |el| el.child(div().flex_none().child("▾"))),
                    )
                    .child(div().w_full().line_clamp(1).text_ellipsis().line_height(px(16.)).text_size(px(12.)).text_color(if alert { theme::red() } else { theme::dim() }).child(SharedString::from(details.join(" · "))))
                    .when_some(used, |el, used| el.child(div().mt(px(4.)).w_full().h(px(3.)).rounded(px(2.)).bg(theme::line()).child(div().h_full().rounded(px(2.)).w(gpui::relative(used)).bg(if alert { theme::red() } else { theme::accent() })))),
            )
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w(px(22.))
                    .h(px(22.))
                    .flex()
                    .justify_center()
                    .when(index < 9, |el| el.child(div().text_size(px(12.)).text_color(theme::dim()).group_hover(group.clone(), |s| s.invisible()).child((index + 1).to_string())))
                    .child(
                        // Faded rather than hidden: GPUI gives a hidden element no click target, even once hover reveals it.
                        div()
                            .id(("close-thread", index))
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .text_color(theme::dim())
                            .cursor_pointer()
                            .opacity(0.)
                            .group_hover(group.clone(), |s| s.opacity(1.))
                            .hover(|s| s.bg(theme::overlay()).text_color(theme::text()))
                            .on_click(cx.listener(move |root, _, _, cx| {
                                cx.stop_propagation();
                                root.app.close_window(index);
                                root.after_change(cx);
                            }))
                            .child("×"),
                    ),
            )
    }

    fn drag_thread_over(&mut self, index: usize, event: &DragMoveEvent<ThreadDrag>, cx: &mut Context<Self>) {
        let dragged = event.drag(cx).0;
        if self.dragged_thread != Some(dragged) {
            self.dragged_thread = Some(dragged);
            cx.notify();
        }
        let Some(from) = self.app.windows.iter().position(|w| w.id == dragged) else { return };
        if passes_row(from, index, event.event.position.y, event.bounds.top(), event.bounds.bottom()) {
            self.app.move_window(from, index);
            cx.notify();
        }
    }

    fn drag_scrolls_threads(&mut self, event: &DragMoveEvent<ThreadDrag>, cx: &mut Context<Self>) {
        let y = event.event.position.y;
        let step = if y < event.bounds.top() + px(DRAG_SCROLL_EDGE) {
            px(DRAG_SCROLL_STEP)
        } else if y > event.bounds.bottom() - px(DRAG_SCROLL_EDGE) {
            -px(DRAG_SCROLL_STEP)
        } else {
            return;
        };
        let offset = self.threads_scroll.offset();
        self.threads_scroll.set_offset(point(offset.x, offset.y + step));
        cx.notify();
    }

    pub(super) fn sidebar_view(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = (0..self.app.windows.len())
            .filter(|index| !matches!(&self.app.windows[*index].pane, Pane::Chat(chat) if crate::obiview::is_obiview(chat)))
            .map(|index| {
                let row = {
                    let window = &self.app.windows[index];
                    self.thread_row(index, window, cx)
                };
                row.into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(252.))
            .h_full()
            .bg(theme::surface())
            .border_r_1()
            .border_color(theme::line())
            .child(self.obi_row(cx))
            .child(
                div().flex().flex_none().items_center().justify_between().h(px(36.)).pl(px(14.)).pr(px(8.)).child(div().text_size(px(12.)).text_color(theme::dim()).child("Threads")).child(
                    control("new-chat", "+ New", theme::accent()).on_click(cx.listener(|root, _, _, cx| {
                        crate::commands::choose_new_chat_account(&mut root.app);
                        cx.notify();
                    })),
                ),
            )
            .child(
                div()
                    .id("threads")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .px(px(6.))
                    .overflow_y_scroll()
                    .track_scroll(&self.threads_scroll)
                    .on_drag_move(cx.listener(|root, event: &DragMoveEvent<ThreadDrag>, _, cx| root.drag_scrolls_threads(event, cx)))
                    .children(rows),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .gap_1()
                    .p(px(8.))
                    .border_t_1()
                    .border_color(theme::line())
                    .child(self.mirror_switch(cx))
                    .child(
                        div()
                            .flex()
                            .gap(px(2.))
                            .child(icon_control("open-terminal", TERMINAL_ICON, "Terminal  Alt+T").on_click(cx.listener(|root, _, _, cx| root.run_command("/term", cx))))
                            .child(icon_control("open-sessions", HISTORY_ICON, "Past sessions").on_click(cx.listener(|root, _, _, cx| root.run_command("/sessions", cx))))
                            .child(icon_control("open-nodes", NETWORK_ICON, "Network models").on_click(cx.listener(|root, _, _, cx| root.run_command("/nodes", cx))))
                            .child(icon_control("open-palette", SEARCH_ICON, "Find anything  Alt+K").on_click(cx.listener(|root, _, _, cx| {
                                root.app.picker = Some(crate::commands::palette(&root.app));
                                cx.notify();
                            })))
                            .child(icon_control("self-improve", WRENCH_ICON, "Self-improve").on_click(cx.listener(|root, _, _, cx| root.run_command("/self-improve", cx)))),
                    ),
            )
    }

    pub(super) fn footer(&mut self, width: f32, show_meters: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let hints: &[&str] = if self.app.active_is_chat() {
            &["Enter send", "Ctrl+J new line", "Esc stop", "Alt+K find", "Alt+D changes", "Alt+1-9 switch", "Alt+G grid", "Alt+W close"]
        } else {
            &["Alt+1-9 switch", "Alt+K find", "Alt+G grid", "Alt+W close", "Ctrl+Shift+C copy"]
        };
        let status: Option<SharedString> = self.flash.clone().or_else(|| (!self.app.status.is_empty()).then(|| self.app.status.clone().into()));
        let limits = if show_meters { self.app.rt.limits.snapshot() } else { Vec::new() };
        let usage = self.app.windows.get(self.app.active).and_then(|w| match &w.pane {
            Pane::Chat(chat) => Some(chat.state.usage.weighted()),
            Pane::Term(_) => None,
        });
        let char_width = 7.3;
        let right_chars = status.as_ref().map_or(0, |s| s.chars().count() + 3) + limits.len() * 24 + usage.filter(|u| *u > 0).map_or(0, |_| 14);
        let mut room = (width / char_width) as usize;
        room = room.saturating_sub(right_chars + 4);
        let mut shown = String::new();
        for hint in hints {
            let extra = hint.chars().count() + if shown.is_empty() { 0 } else { 5 };
            if shown.chars().count() + extra > room {
                break;
            }
            if !shown.is_empty() {
                shown.push_str("  ·  ");
            }
            shown.push_str(hint);
        }
        let meters = limits.into_iter().map(|limit| {
            let percent = (limit.used * 100.0).round() as u32;
            let hot = limit.blocked || percent >= 90;
            let color = if hot { theme::red() } else { theme::dim() };
            let label = format!("{} {} {percent}%{}", limit.vendor, window_word(&limit.window), if limit.blocked { " limit hit" } else { "" });
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .text_color(color)
                .child(label)
                .child(div().w(px(32.)).h(px(4.)).rounded(px(2.)).bg(theme::line()).child(div().h_full().rounded(px(2.)).w(px(32. * limit.used.clamp(0.0, 1.0) as f32)).bg(if hot { theme::red() } else { theme::accent() })))
        });
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap_4()
            .h(px(26.))
            .px(px(14.))
            .bg(theme::surface())
            .border_t_1()
            .border_color(theme::line())
            .text_size(px(12.))
            .text_color(theme::dim())
            .when(show_meters, |el| el.child(self.mirror_switch(cx)))
            .child(div().min_w_0().whitespace_nowrap().overflow_hidden().child(shown))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_4()
                    .when_some(status, |el, status| el.child(div().flex_none().text_color(theme::accent()).child(status)))
                    .children(meters)
                    .when_some(usage.filter(|u| *u > 0), |el, usage| el.child(div().flex_none().child(format!("{} tokens", format_tokens(usage))))),
            )
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dragged_thread_swaps_past_the_middle_of_a_row_and_never_back() {
        let (top, bottom) = (px(100.), px(200.));
        assert!(!passes_row(0, 1, px(140.), top, bottom));
        assert!(passes_row(0, 1, px(160.), top, bottom));
        assert!(!passes_row(2, 1, px(160.), top, bottom));
        assert!(passes_row(2, 1, px(140.), top, bottom));
        assert!(!passes_row(1, 1, px(160.), top, bottom));
        assert!(!passes_row(0, 1, px(260.), top, bottom), "a row the cursor isn't on");
        // A short row just moved below a tall one: the cursor is still past the tall row's new middle.
        assert!(!passes_row(1, 0, px(160.), px(40.), px(140.)));
    }

    #[test]
    fn tokens_and_paths_read_compactly() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(54_100), "54.1k");
        assert_eq!(format_tokens(2_500_000), "2.5M");
        assert_eq!(short_path(&user_home()), "~");
        assert_eq!(short_path(&user_home().join("x")), "~\\x");
        assert_eq!(path_tail("C:\\a\\project", 50), "C:\\a\\project");
        assert_eq!(path_tail("C:\\a\\project", 8), "…project");
    }

    #[test]
    fn each_thread_names_who_it_is_connected_to_and_whose_quota_it_uses() {
        let home = Path::new("C:/u");
        let default_dir = home.join(".claude");
        let work = Path::new("C:/u/.claude-work");
        let labels = HashMap::from([(default_dir.clone(), "me@example.com (max)".to_string()), (work.to_path_buf(), "you@example.com (pro)".to_string()), (home.join(".codex"), "me@example.com (pro)".to_string()), (home.join(".codex-team"), "team@example.com (plus)".to_string())]);
        let default = thread_card(Some(&Provider::Claude), None, &labels, home);
        assert_eq!((default.who.as_str(), default.vendor.as_deref()), ("me@example.com (max)", Some("claude")));
        let other = thread_card(Some(&Provider::Claude), Some(work), &labels, home);
        assert_eq!((other.who.as_str(), other.vendor.as_deref()), ("work · you@example.com (pro)", Some("claude:work")));
        let unknown = thread_card(Some(&Provider::Claude), Some(Path::new("C:/u/.claude-new")), &labels, home);
        assert_eq!(unknown.who, "Claude new");
        let codex = thread_card(Some(&Provider::Codex), None, &labels, home);
        assert_eq!((codex.who.as_str(), codex.vendor.as_deref()), ("me@example.com (pro)", Some("codex")));
        let team = thread_card(Some(&Provider::Codex), Some(Path::new("C:/u/.codex-team")), &labels, home);
        assert_eq!((team.who.as_str(), team.vendor.as_deref()), ("team · team@example.com (plus)", Some("codex:team")));
        let unnamed = thread_card(Some(&Provider::Codex), None, &HashMap::new(), home);
        assert_eq!(unnamed.who, "ChatGPT");
        let node = thread_card(Some(&Provider::Node("RIG".into())), None, &labels, home);
        assert_eq!((node.who.as_str(), node.vendor), ("RIG", None));
        assert_eq!(thread_card(None, None, &labels, home).who, "Chooses per message");
    }

    #[test]
    fn threads_show_the_mark_of_who_they_run_on() {
        assert_eq!(provider_mark(Some(&Provider::Claude)), Some(CLAUDE_MARK));
        assert_eq!(provider_mark(Some(&Provider::Codex)), Some(OPENAI_MARK));
        assert_eq!(provider_mark(Some(&Provider::Node("x".into()))), None);
        assert_eq!(provider_mark(None), None);
        assert_eq!(terminal_mark("Claude Code"), Some(CLAUDE_MARK));
        assert_eq!(terminal_mark("Sign in: work"), Some(CLAUDE_MARK));
        assert_eq!(terminal_mark("Codex"), Some(OPENAI_MARK));
        assert_eq!(terminal_mark("Codex sign in: team"), Some(OPENAI_MARK));
        assert_eq!(terminal_mark("PowerShell"), None);
    }

    #[test]
    fn claude_is_orange_and_chatgpt_is_white() {
        assert_eq!(mark_color(CLAUDE_MARK), theme::claude());
        assert_eq!(mark_color(OPENAI_MARK), theme::accent());
    }

    #[test]
    fn auto_shows_what_it_chose() {
        let target = olympus_core::target::Target::parse("claude:sonnet").expect("target");
        assert_eq!(model_label(&Choice::Auto, Some(&target)), "auto · claude:sonnet");
        assert_eq!(model_label(&Choice::Auto, None), "auto");
        assert_eq!(model_label(&Choice::Fixed(target), None), "claude:sonnet");
    }
}
