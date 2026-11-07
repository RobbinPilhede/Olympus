use super::parts::Part;
use super::{control, primary_control, Root, FONT, JARVIS_MARK, LINE};
use crate::obiview::{self, AppMode, ReadKind, Tab};
use crate::session::Activity;
use crate::theme;
use gpui::prelude::*;
use gpui::{canvas, div, fill, font, point, px, size, AnyElement, Bounds, Context, FontWeight, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, ScrollWheelEvent, SharedString, StyledText, TextRun, Window};
use olympus_obi::graph::{label_alpha, node_radius};
use olympus_obi::vault::NodeKind;
use std::rc::Rc;

const NAV_WIDTH: f32 = 220.0;
/// How wide Jarvis's column is beside your notes.
pub const CHAT_WIDTH: f32 = 380.0;
const LABEL_FADE: f32 = 1.0;
const MAX_ROWS: usize = 400;
/// A block number no chat reaches, so a note's text can't be confused with a transcript's when selecting.
const NOTE_BLOCK: usize = usize::MAX / 2;

fn local_time(stamp: i64) -> String {
    chrono::DateTime::from_timestamp(stamp, 0).map(|time| time.with_timezone(&chrono::Local).format("%H:%M").to_string()).unwrap_or_default()
}

fn node_color(kind: &NodeKind, focus: bool) -> gpui::Hsla {
    let base = match kind {
        NodeKind::Note => theme::text(),
        NodeKind::Tag => theme::dim(),
        NodeKind::Unresolved => theme::line(),
        NodeKind::Attachment => theme::dim(),
    };
    theme::tint(base, if focus { 1.0 } else { 0.22 })
}

fn to_screen(position: (f32, f32), view: &obiview::GraphView, centre: Point<Pixels>) -> Point<Pixels> {
    point(centre.x + px((position.0 + view.pan.0) * view.zoom), centre.y + px((position.1 + view.pan.1) * view.zoom))
}

fn nearest_node(positions: &[(f32, f32)], degree: &[usize], view: &obiview::GraphView, centre: Point<Pixels>, mouse: Point<Pixels>) -> Option<usize> {
    let mut best = None;
    let mut closest = f32::MAX;
    for (index, position) in positions.iter().enumerate() {
        let at = to_screen(*position, view, centre);
        let (dx, dy) = (f32::from(at.x - mouse.x), f32::from(at.y - mouse.y));
        let distance = (dx * dx + dy * dy).sqrt();
        let reach = (node_radius(degree.get(index).copied().unwrap_or(0), 1.0) * view.zoom.sqrt()).max(7.0);
        if distance <= reach && distance < closest {
            closest = distance;
            best = Some(index);
        }
    }
    best
}

impl Root {
    /// The pinned thread at the top of the sidebar, with its switch.
    pub(super) fn obi_row(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(index) = obiview::index(&self.app) else { return div().into_any_element() };
        let focused = index == self.app.active;
        let reading = self.app.obi.observer.is_some();
        let running = self.app.windows[index].activity == Activity::Running;
        let line: SharedString = self.app.obi.status_line().into();
        div()
            .id("obi-row")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .mx(px(6.))
            .mt(px(6.))
            .px(px(8.))
            .py(px(7.))
            .rounded(px(7.))
            .cursor_pointer()
            .border_1()
            .border_color(if focused { theme::tint(theme::accent(), 0.45) } else { theme::line() })
            .when(focused, |el| el.bg(theme::raised()))
            .when(!focused, |el| el.hover(|s| s.bg(theme::raised())))
            .on_click(cx.listener(move |root, _, _, cx| {
                root.app.focus(index);
                cx.notify();
            }))
            .child(gpui::svg().path(JARVIS_MARK).flex_none().size(px(16.)).text_color(if running || reading { theme::accent() } else { theme::dim() }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().line_height(px(18.)).text_size(px(12.5)).font_weight(FontWeight::MEDIUM).child("Obiview"))
                    .child(div().w_full().line_clamp(1).text_ellipsis().line_height(px(16.)).text_size(px(12.)).text_color(theme::dim()).child(line)),
            )
            .child(
                div()
                    .id("obi-switch")
                    .flex()
                    .flex_none()
                    .items_center()
                    .w(px(32.))
                    .h(px(18.))
                    .p(px(2.))
                    .rounded(px(9.))
                    .bg(if reading { theme::accent() } else { theme::line() })
                    .when(reading, |el| el.justify_end())
                    .on_click(cx.listener(|root, _, _, cx| {
                        cx.stop_propagation();
                        obiview::toggle(&mut root.app);
                        root.after_change(cx);
                    }))
                    .child(div().size(px(14.)).rounded_full().bg(if reading { theme::bg() } else { theme::text() })),
            )
            .into_any_element()
    }

    fn obi_tab(&mut self, id: &'static str, label: &'static str, tab: Tab, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.app.obi.tab == tab;
        control(id, label, if on { theme::accent() } else { theme::dim() }).when(on, |el| el.bg(theme::raised())).on_click(cx.listener(move |root, _, _, cx| {
            if tab == Tab::Graph {
                obiview::open_graph(&mut root.app, None);
            } else {
                root.app.obi.tab = tab;
            }
            cx.notify();
        }))
    }

    fn obi_nav(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(vault) = &self.app.obi.vault else { return div().child("Your vault isn't open.").into_any_element() };
        let open = self.app.obi.note.as_ref().map(|note| note.rel.clone());
        let mut notes: Vec<(String, String)> = vault.notes().iter().map(|note| (note.rel.clone(), note.title.clone())).collect();
        notes.sort();
        let tags = vault.tags();
        let rows: Vec<AnyElement> = notes
            .into_iter()
            .take(MAX_ROWS)
            .enumerate()
            .map(|(index, (rel, title))| {
                let on = open.as_deref() == Some(rel.as_str());
                let folder: SharedString = rel.rsplit_once('/').map_or_else(String::new, |(folder, _)| folder.to_string()).into();
                let has_folder = !folder.is_empty();
                div()
                    .id(("obi-note", index))
                    .flex()
                    .flex_col()
                    .flex_none()
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .when(on, |el| el.bg(theme::raised()))
                    .hover(|s| s.bg(theme::raised()))
                    .on_click(cx.listener(move |root, _, _, cx| {
                        obiview::save_note(&mut root.app);
                        obiview::open_note(&mut root.app, &rel);
                        cx.notify();
                    }))
                    .child(div().line_clamp(1).text_ellipsis().text_size(px(12.5)).line_height(px(18.)).child(SharedString::from(title)))
                    .when(has_folder, |el| el.child(div().line_clamp(1).text_ellipsis().text_size(px(12.)).line_height(px(15.)).text_color(theme::dim()).child(folder)))
                    .into_any_element()
            })
            .collect();
        let tag_rows: Vec<AnyElement> = tags
            .into_iter()
            .take(60)
            .enumerate()
            .map(|(index, (tag, count))| {
                let query = format!("tag:{tag}");
                div()
                    .id(("obi-tag", index))
                    .flex()
                    .flex_none()
                    .justify_between()
                    .px(px(8.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(theme::dim())
                    .hover(|s| s.bg(theme::raised()))
                    .on_click(cx.listener(move |root, _, _, cx| {
                        root.app.obi.search.clone_from(&query);
                        root.app.obi.tab = Tab::Search;
                        cx.notify();
                    }))
                    .child(div().line_clamp(1).text_ellipsis().child(SharedString::from(format!("#{tag}"))))
                    .child(div().flex_none().child(count.to_string()))
                    .into_any_element()
            })
            .collect();
        div()
            .id("obi-nav")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(NAV_WIDTH))
            .h_full()
            .gap_1()
            .p(px(6.))
            .bg(theme::surface())
            .border_r_1()
            .border_color(theme::line())
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap_1()
                    .flex_wrap()
                    .child(primary_control("obi-today-note", "Today's note").on_click(cx.listener(|root, _, _, cx| {
                        let rel = olympus_obi::vault::daily_rel(chrono::Local::now().date_naive());
                        obiview::open_note(&mut root.app, &rel);
                        cx.notify();
                    })))
                    .child(control("obi-new-note", "New note", theme::dim()).on_click(cx.listener(|root, _, _, cx| {
                        root.app.obi.searching = false;
                        root.app.composer.set("/note ");
                        cx.notify();
                    }))),
            )
            .child(div().flex_none().mt(px(6.)).px(px(8.)).text_size(px(12.)).text_color(theme::dim()).child("Notes"))
            .children(rows)
            .when(!tag_rows.is_empty(), |el| el.child(div().flex_none().mt(px(8.)).px(px(8.)).text_size(px(12.)).text_color(theme::dim()).child("Tags")))
            .children(tag_rows)
            .into_any_element()
    }

    fn obi_read_row(&mut self, id: &'static str, label: &'static str, hint: &'static str, kind: ReadKind, on: bool, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(div().flex().flex_col().min_w_0().child(div().child(label)).child(div().text_color(theme::dim()).text_size(px(12.)).child(hint)))
            .child(
                div()
                    .id(id)
                    .flex()
                    .flex_none()
                    .items_center()
                    .w(px(32.))
                    .h(px(18.))
                    .p(px(2.))
                    .rounded(px(9.))
                    .cursor_pointer()
                    .bg(if on { theme::accent() } else { theme::line() })
                    .when(on, |el| el.justify_end())
                    .on_click(cx.listener(move |root, _, _, cx| {
                        obiview::set_read(&mut root.app, kind, !on);
                        root.after_change(cx);
                    }))
                    .child(div().size(px(14.)).rounded_full().bg(if on { theme::bg() } else { theme::text() })),
            )
            .into_any_element()
    }

    fn obi_app_row(&mut self, index: usize, app: crate::app_list::AppEntry, mode: AppMode, cx: &mut Context<Self>) -> AnyElement {
        let letter: SharedString = app.name.chars().next().unwrap_or('?').to_uppercase().to_string().into();
        let icon = div().flex().flex_none().items_center().justify_center().size(px(24.)).rounded(px(5.)).overflow_hidden();
        let icon = match app.icon {
            Some(path) => icon.child(gpui::img(path).size_full()),
            None => icon.bg(theme::raised()).text_size(px(12.)).child(letter),
        };
        let name = div().flex_1().min_w_0().line_clamp(1).text_ellipsis().child(SharedString::from(app.name));
        let row = div().flex().items_center().gap(px(10.)).min_h(px(30.)).child(icon).child(name);
        if app.locked {
            return row.child(div().flex_none().text_color(theme::dim()).text_size(px(12.)).child("Never read")).into_any_element();
        }
        let choices = [("obi-app-read", "Read", AppMode::Read), ("obi-app-time", "Time only", AppMode::TimeOnly), ("obi-app-off", "Off", AppMode::Off)];
        let mut buttons = div().flex().flex_none().items_center().gap(px(2.));
        for (id, label, choice) in choices {
            let exe = app.exe.clone();
            buttons = buttons.child(
                control((id, index), label, if mode == choice { theme::accent() } else { theme::dim() })
                    .when(mode == choice, |el| el.bg(theme::raised()))
                    .on_click(cx.listener(move |root, _, _, cx| {
                        obiview::set_app(&mut root.app, &exe, choice);
                        root.after_change(cx);
                    })),
            );
        }
        row.child(buttons).into_any_element()
    }

    fn obi_apps(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.app.obi.apps.is_empty() {
            obiview::refresh_apps(&mut self.app);
        }
        let saved = self.app.obi.saved.clone();
        let loading = self.app.obi.apps_loading && self.app.obi.apps.is_empty();
        let apps = self.app.obi.apps.clone();
        let sites = obiview::site_rows(&self.app);
        let only = !saved.read_every_app;
        let heading = |text: &'static str| div().mt(px(6.)).font_weight(FontWeight::BOLD).child(text);
        let scope = |id: &'static str, label: &'static str, on: bool, value: bool, cx: &mut Context<Self>| {
            control(id, label, if on { theme::accent() } else { theme::dim() }).when(on, |el| el.bg(theme::raised())).on_click(cx.listener(move |root, _, _, cx| {
                obiview::set_only_picked(&mut root.app, value);
                root.after_change(cx);
            }))
        };
        let reads = vec![
            self.obi_read_row("obi-read-text", "Text in apps", "What's shown on screen, so you can search it and summarize it.", ReadKind::Text, saved.read_text, cx),
            self.obi_read_row("obi-read-behind", "Windows behind the one you're using", "Checked once a minute, so mail and chats that come in get noticed.", ReadKind::Behind, saved.read_behind, cx),
            self.obi_read_row("obi-read-files", "Files you save", "File names only, in your home folder, plus that day's Git commits.", ReadKind::Files, saved.read_files, cx),
            self.obi_read_row("obi-read-processes", "Programs starting and stopping", "Just which ones and when.", ReadKind::Processes, saved.read_processes, cx),
        ];
        let mut open_rows = Vec::new();
        let mut other_rows = Vec::new();
        for (index, app) in apps.into_iter().enumerate() {
            let is_open = app.open;
            let row = self.obi_app_row(index, app.clone(), saved.mode_of(&app.exe), cx);
            if is_open { open_rows.push(row) } else { other_rows.push(row) }
        }
        let site_rows: Vec<AnyElement> = sites
            .into_iter()
            .enumerate()
            .map(|(index, (name, read))| {
                let target = name.clone();
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .min_h(px(30.))
                    .child(div().min_w_0().line_clamp(1).text_ellipsis().when(!read, |el| el.text_color(theme::dim())).child(SharedString::from(name)))
                    .child(control(("obi-site", index), if read { "Stop reading" } else { "Read again" }, theme::dim()).on_click(cx.listener(move |root, _, _, cx| {
                        obiview::set_site(&mut root.app, &target, !read);
                        root.after_change(cx);
                    })))
                    .into_any_element()
            })
            .collect();
        div()
            .id("obi-apps")
            .flex()
            .flex_col()
            .size_full()
            .gap(px(8.))
            .p(px(18.))
            .overflow_y_scroll()
            .child(div().text_size(px(19.)).line_height(px(27.)).font_weight(FontWeight::BOLD).child("What Obiview reads"))
            .child(div().text_color(theme::dim()).text_size(px(12.)).child("Every app starts off. Read keeps what's on screen. Time only keeps how long you used an app and its window titles. Off keeps nothing. Changes apply to new reading: what it already read stays for up to 14 days, and Jarvis can still see it."))
            .child(div().text_color(theme::dim()).text_size(px(12.)).child("Always skipped: password fields, password managers, private browsing and banks."))
            .child(heading("What to read"))
            .children(reads)
            .child(heading("Apps"))
            .child(div().flex().flex_wrap().gap(px(4.)).child(scope("obi-scope-all", "Every app except those I turn off", !only, false, cx)).child(scope("obi-scope-only", "Only the apps I pick", only, true, cx)))
            .when(!saved.has_chosen_apps(), |el| el.child(div().text_color(theme::bright()).text_size(px(12.)).child("Nothing is read until you choose Read or Time only on an app.")))
            .when(loading, |el| el.child(div().text_color(theme::dim()).child("Looking for your apps...")))
            .when(!open_rows.is_empty(), |el| el.child(div().mt(px(4.)).text_color(theme::dim()).text_size(px(12.)).child("Open now")).children(open_rows))
            .when(!other_rows.is_empty(), |el| el.child(div().mt(px(4.)).text_color(theme::dim()).text_size(px(12.)).child("Installed")).children(other_rows))
            .child(heading("Inside your browser"))
            .child(div().text_color(theme::dim()).text_size(px(12.)).child("Sites appear here once you've used them. To skip one before that, run /obiview exclude and its name."))
            .children(site_rows)
            .into_any_element()
    }

    fn obi_today(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let reading = self.app.obi.observer.is_some();
        let status = self.app.obi.observer.as_ref().map(olympus_obi::capture::Observer::status);
        let problem = self.app.obi.last_problem.clone().or_else(|| status.as_ref().and_then(|status| status.last_error.clone()));
        let written = self.app.obi.last_written.clone();
        let summary_model = self.app.obi.saved.summary_model();
        let summaries: SharedString = summary_model.as_ref().map_or_else(|| "Hold Shift+B and talk to Jarvis. Nothing is tidied in the background until you choose an app.".to_string(), |model| if self.app.obi.saved.summary_messages { format!("{model} tidies your notes from what Obiview reads, including mail and chats, on your subscription.") } else { format!("{model} tidies your notes from what Obiview reads, on your subscription. Mail, chats and meetings stay out.") }).into();
        let excluded = self.app.obi.saved.excluded.clone();
        let store = obiview::open_store();
        let sources = store.as_ref().and_then(|store| store.sources().ok()).unwrap_or_default();
        let timeline = obiview::timeline_runs(&store.as_ref().and_then(|store| store.day_timeline(chrono::Local::now().date_naive()).ok()).unwrap_or_default());
        let source_rows: Vec<AnyElement> = sources
            .into_iter()
            .filter(|source| !excluded.iter().any(|word| source.source.to_lowercase().contains(&word.to_lowercase())))
            .take(24)
            .enumerate()
            .map(|(index, source)| {
                let command = format!("/obiview exclude {}", source.source);
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(div().min_w_0().line_clamp(1).text_ellipsis().child(SharedString::from(source.source)))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(10.))
                            .child(div().text_color(theme::dim()).text_size(px(12.)).child(SharedString::from(format!("{} read, last {}", source.captures, local_time(source.last_seen)))))
                            .child(control(("obi-stop", index), "Stop reading", theme::dim()).on_click(cx.listener(move |root, _, _, cx| root.run_command(&command, cx)))),
                    )
                    .into_any_element()
            })
            .collect();
        let excluded_rows: Vec<AnyElement> = excluded
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let command = format!("/obiview include {name}");
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(div().min_w_0().line_clamp(1).text_ellipsis().text_color(theme::dim()).child(SharedString::from(name.clone())))
                    .child(control(("obi-read-again", index), "Read again", theme::dim()).on_click(cx.listener(move |root, _, _, cx| root.run_command(&command, cx))))
                    .into_any_element()
            })
            .collect();
        let timeline_rows: Vec<AnyElement> = timeline
            .into_iter()
            .rev()
            .take(80)
            .map(|row| {
                div()
                    .flex()
                    .gap(px(10.))
                    .child(div().flex_none().w(px(44.)).text_color(theme::dim()).child(SharedString::from(local_time(row.start))))
                    .child(div().flex_none().w(px(120.)).line_clamp(1).text_ellipsis().text_color(theme::bright()).child(SharedString::from(row.source.unwrap_or(row.app))))
                    .child(div().flex_1().min_w_0().line_clamp(1).text_ellipsis().child(SharedString::from(row.title)))
                    .into_any_element()
            })
            .collect();
        let empty = timeline_rows.is_empty();
        div()
            .id("obi-today")
            .flex()
            .flex_col()
            .size_full()
            .gap(px(16.))
            .p(px(18.))
            .overflow_y_scroll()
            .child(div().flex().flex_col().gap_1().child(div().text_size(px(19.)).line_height(px(27.)).font_weight(FontWeight::BOLD).child("Today")).child(div().text_color(theme::dim()).text_size(px(12.)).child(summaries)))
            .child(div().flex().child(control("obi-choose-apps", "Choose what it reads", theme::text()).on_click(cx.listener(|root, _, _, cx| root.run_command("/obiview apps", cx)))))
            .when_some(status.as_ref().map(|status| status.stored), |el, stored| el.child(div().text_color(theme::dim()).child(SharedString::from(match stored {
                1 => "1 thing read since it started.".to_string(),
                count => format!("{count} things read since it started."),
            }))))
            .when_some(problem, |el, problem| el.child(div().px(px(10.)).py(px(8.)).rounded(px(8.)).bg(theme::tint(theme::red(), 0.1)).text_color(theme::red()).child(SharedString::from(problem))))
            .when_some(written, |el, written| el.child(div().text_color(theme::dim()).child(SharedString::from(written))))
            .when(!reading, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .p(px(12.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(theme::line())
                        .child(div().font_weight(FontWeight::BOLD).child("Obiview isn't reading yet"))
                        .child(div().text_color(theme::dim()).child("Every app starts off. Turn this on, then pick apps on the Apps tab, like Teams, Outlook or your browser. What it reads stays on this PC. It skips password fields, password managers, private browsing and banks. Your notes work either way."))
                        .child(div().flex().child(primary_control("obi-start", "Start reading").on_click(cx.listener(|root, _, _, cx| {
                            obiview::toggle(&mut root.app);
                            root.after_change(cx);
                        })))),
                )
            })
            .when(self.app.obi.saved.summaries_off, |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .p(px(12.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(theme::line())
                        .child(div().font_weight(FontWeight::BOLD).child("Add a written summary"))
                        .child(div().text_color(theme::dim()).child("Haiku can turn what Obiview reads into a short summary of your day, rewritten every 15 minutes. It runs on your connected Claude subscription and uses no API credits. That text goes to Claude. Mail, chats and meetings stay out unless you run /obiview summaries haiku messages."))
                        .child(div().flex().child(primary_control("obi-summaries", "Write summaries with Haiku").on_click(cx.listener(|root, _, _, cx| root.run_command("/obiview summaries haiku", cx))))),
                )
            })
            .when(!source_rows.is_empty(), |el| el.child(div().flex().flex_col().gap_1().child(div().font_weight(FontWeight::BOLD).child("What it's reading")).children(source_rows)))
            .when(!excluded_rows.is_empty(), |el| el.child(div().flex().flex_col().gap_1().child(div().font_weight(FontWeight::BOLD).child("Not reading")).children(excluded_rows)))
            .child(div().flex().flex_col().gap_1().child(div().font_weight(FontWeight::BOLD).child("Your day")).when(empty, |el| el.child(div().text_color(theme::dim()).child("Nothing yet today."))).children(timeline_rows))
            .into_any_element()
    }

    fn obi_note(&mut self, id: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some(note) = self.app.obi.note.clone() else {
            return div().p(px(18.)).text_color(theme::dim()).child("Choose a note on the left, or open today's note.").into_any_element();
        };
        let editing = self.app.obi.editing;
        let head = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .h(px(36.))
            .px(px(14.))
            .border_b_1()
            .border_color(theme::line())
            .child(div().flex_1().min_w_0().line_clamp(1).text_ellipsis().text_size(px(12.)).text_color(theme::dim()).child(SharedString::from(note.rel.clone())))
            .child(control("obi-local-graph", "Local graph", theme::dim()).on_click(cx.listener({
                let rel = note.rel.clone();
                move |root, _, _, cx| {
                    obiview::open_graph(&mut root.app, Some((rel.clone(), 2)));
                    cx.notify();
                }
            })))
            .child(if editing {
                primary_control("obi-save", "Save").on_click(cx.listener(|root, _, _, cx| {
                    obiview::save_note(&mut root.app);
                    obiview::refresh_open(&mut root.app);
                    cx.notify();
                }))
            } else {
                control("obi-edit", "Edit", theme::accent()).on_click(cx.listener(|root, _, _, cx| {
                    obiview::begin_edit(&mut root.app);
                    cx.notify();
                }))
            });
        let body = if editing {
            self.obi_editor(cx)
        } else {
            let parts: Vec<Part> = obiview::note_parts(&self.app, &note.rel, &note.text);
            let rendered: Vec<AnyElement> = parts.iter().enumerate().map(|(index, part)| self.part_el(id, NOTE_BLOCK, index, part, cx)).collect();
            let vault = self.app.obi.vault.as_ref();
            let backlinks = vault.map(|vault| vault.backlinks(&note.rel)).unwrap_or_default();
            let mentions = vault.map(|vault| vault.unlinked_mentions(&note.rel)).unwrap_or_default();
            let link_rows = |prefix: &'static str, links: Vec<olympus_obi::vault::Backlink>, cx: &mut Context<Self>| -> Vec<AnyElement> {
                links
                    .into_iter()
                    .take(50)
                    .enumerate()
                    .map(|(index, link)| {
                        let from = link.from.clone();
                        div()
                            .id(SharedString::from(format!("{prefix}-{index}")))
                            .flex()
                            .flex_col()
                            .px(px(8.))
                            .py(px(4.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::raised()))
                            .on_click(cx.listener(move |root, _, _, cx| {
                                obiview::open_note(&mut root.app, &from);
                                cx.notify();
                            }))
                            .child(div().text_color(theme::bright()).child(SharedString::from(link.from)))
                            .child(div().text_size(px(12.)).text_color(theme::dim()).line_clamp(2).child(SharedString::from(link.context)))
                            .into_any_element()
                    })
                    .collect()
            };
            let empty_note = note.text.trim().is_empty();
            let backlink_rows = link_rows("obi-back", backlinks, cx);
            let mention_rows = link_rows("obi-mention", mentions, cx);
            div()
                .id("obi-note-body")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap(px(8.))
                .p(px(18.))
                .overflow_y_scroll()
                .when(empty_note, |el| el.child(div().text_color(theme::dim()).child("This note is empty. Choose Edit to write in it.")))
                .children(rendered)
                .child(div().mt(px(14.)).h(px(1.)).flex_none().bg(theme::line()))
                .child(div().flex_none().font_weight(FontWeight::BOLD).child(SharedString::from(format!("Linked here ({})", backlink_rows.len()))))
                .children(backlink_rows)
                .when(!mention_rows.is_empty(), |el| el.child(div().flex_none().font_weight(FontWeight::BOLD).child(SharedString::from(format!("Mentioned without a link ({})", mention_rows.len())))))
                .children(mention_rows)
                .into_any_element()
        };
        div().flex().flex_col().size_full().child(head).child(body).into_any_element()
    }

    fn obi_editor(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let editor = &self.app.obi.editor;
        let shown: SharedString = if editor.text.is_empty() { " ".to_string().into() } else { editor.text.clone().into() };
        let len = shown.len();
        let run = TextRun { len, font: font(FONT), color: theme::text(), background_color: None, underline: None, strikethrough: None };
        let styled = StyledText::new(shown).with_runs(vec![run]);
        let layout = styled.layout().clone();
        self.obi_editor_layout = Some(layout.clone());
        let cursor = editor.cursor;
        let caret = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let at = layout.position_for_index(cursor).unwrap_or(bounds.origin);
                window.paint_quad(fill(Bounds::new(at, size(px(2.), px(LINE))), theme::accent()));
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        div()
            .id("obi-editor")
            .flex_1()
            .min_h_0()
            .p(px(18.))
            .overflow_y_scroll()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|root, event: &MouseDownEvent, _, cx| {
                    if let Some(layout) = &root.obi_editor_layout {
                        let index = layout.index_for_position(event.position).unwrap_or_else(|nearest| nearest);
                        root.app.obi.editor.move_to(index, event.modifiers.shift);
                    }
                    cx.notify();
                }),
            )
            .child(div().relative().child(styled).child(caret))
            .into_any_element()
    }

    fn obi_search(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.app.obi.search.clone();
        let (hits, read) = if query.trim().is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let hits = self.app.obi.vault.as_ref().map(|vault| vault.search(&query, 60)).unwrap_or_default();
            let read = obiview::open_store().and_then(|store| store.search(&query, 30).ok()).unwrap_or_default();
            (hits, read)
        };
        let count = hits.len();
        let hit_rows: Vec<AnyElement> = hits
            .into_iter()
            .enumerate()
            .map(|(index, hit)| {
                let rel = hit.rel.clone();
                div()
                    .id(("obi-hit", index))
                    .flex()
                    .flex_col()
                    .flex_none()
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::raised()))
                    .on_click(cx.listener(move |root, _, _, cx| {
                        obiview::open_note(&mut root.app, &rel);
                        cx.notify();
                    }))
                    .child(div().text_color(theme::bright()).child(SharedString::from(hit.rel)))
                    .child(div().text_size(px(12.)).text_color(theme::dim()).line_clamp(2).child(SharedString::from(hit.snippet)))
                    .into_any_element()
            })
            .collect();
        let read_rows: Vec<AnyElement> = read
            .into_iter()
            .map(|hit| {
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .px(px(8.))
                    .child(div().text_color(theme::bright()).line_clamp(1).child(SharedString::from(format!("{} {}", local_time(hit.ts), hit.source.unwrap_or(hit.exe)))))
                    .child(div().text_size(px(12.)).text_color(theme::dim()).line_clamp(2).child(SharedString::from(hit.snippet)))
                    .into_any_element()
            })
            .collect();
        let has_query = !query.is_empty();
        let on = self.app.obi.searching;
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .id("obi-query")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(10.))
                    .h(px(40.))
                    .mx(px(14.))
                    .mt(px(10.))
                    .px(px(12.))
                    .rounded(px(8.))
                    .bg(theme::raised())
                    .border_1()
                    .border_color(if on { theme::accent() } else { theme::line() })
                    .cursor_pointer()
                    .on_click(cx.listener(|root, _, _, cx| {
                        root.app.obi.searching = true;
                        cx.notify();
                    }))
                    .child(div().flex_none().text_color(theme::accent()).child("›"))
                    .child(div().flex_1().min_w_0().line_clamp(1).text_ellipsis().text_color(if has_query { theme::text() } else { theme::dim() }).child(if has_query { SharedString::from(query) } else { "Click to search. tag:, file:, path:, [property], -not, \"phrases\"".into() }))
                    .when(on, |el| el.child(div().flex_none().w(px(2.)).h(px(18.)).bg(theme::accent()))),
            )
            .child(
                div()
                    .id("obi-results")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .p(px(14.))
                    .overflow_y_scroll()
                    .when(has_query, |el| el.child(div().flex_none().text_size(px(12.)).text_color(theme::dim()).child(SharedString::from(format!("{count} in your notes")))))
                    .children(hit_rows)
                    .when(!read_rows.is_empty(), |el| el.child(div().flex_none().mt(px(10.)).text_size(px(12.)).text_color(theme::dim()).child("In what Obiview has read")))
                    .children(read_rows),
            )
            .into_any_element()
    }

    fn obi_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(graph) = &self.app.obi.graph else {
            return div().p(px(18.)).text_color(theme::dim()).child("Your graph appears here as notes link to each other.").into_any_element();
        };
        let (positions, settled) = graph.snapshot();
        if !settled {
            window.request_animation_frame();
        }
        let nodes = Rc::new(graph.data.nodes.iter().map(|node| (node.label.clone(), node.kind, node.degree)).collect::<Vec<_>>());
        let edges = Rc::new(graph.data.edges.clone());
        let bounds_slot = self.obi_graph_bounds.clone();
        if !self.app.obi.graph_view.held {
            let area = bounds_slot.borrow().size;
            if f32::from(area.width) > 0.0 {
                (self.app.obi.graph_view.zoom, self.app.obi.graph_view.pan) = obiview::fit_graph(&positions, f32::from(area.width), f32::from(area.height));
            }
        }
        let view = self.app.obi.graph_view;
        let positions = Rc::new(positions);
        let (paint_nodes, paint_edges, paint_positions) = (nodes.clone(), edges, positions.clone());
        let degree: Rc<Vec<usize>> = Rc::new(nodes.iter().map(|node| node.2).collect());
        let hover_positions = positions.clone();
        let hover_degree = degree.clone();
        let hover_bounds = bounds_slot.clone();
        let down_positions = positions.clone();
        let down_degree = degree;
        let down_bounds = bounds_slot.clone();
        let wheel_bounds = bounds_slot.clone();
        let empty = nodes.is_empty();
        div()
            .id("obi-graph")
            .relative()
            .size_full()
            .overflow_hidden()
            .when(empty, |el| el.child(div().p(px(18.)).text_color(theme::dim()).child("Nothing to map yet. Link notes with [[double brackets]].")))
            .child(
                canvas(
                    move |_, _, _| (),
                    move |bounds, _, window, cx| {
                        *bounds_slot.borrow_mut() = bounds;
                        paint_graph(bounds, &paint_nodes, &paint_edges, &paint_positions, &view, window, cx);
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .on_scroll_wheel(cx.listener(move |root, event: &ScrollWheelEvent, _, cx| {
                let delta = f32::from(event.delta.pixel_delta(px(LINE)).y);
                let centre = wheel_bounds.borrow().center();
                obiview::zoom_graph(&mut root.app.obi.graph_view, delta, (f32::from(event.position.x - centre.x), f32::from(event.position.y - centre.y)));
                cx.notify();
            }))
            .on_mouse_move(cx.listener(move |root, event: &MouseMoveEvent, _, cx| {
                let centre = hover_bounds.borrow().center();
                let view = &mut root.app.obi.graph_view;
                view.held |= view.dragging.is_some() || view.panning.is_some();
                if let Some(index) = view.dragging {
                    let at = ((f32::from(event.position.x - centre.x)) / view.zoom - view.pan.0, f32::from(event.position.y - centre.y) / view.zoom - view.pan.1);
                    if let Some(graph) = &root.app.obi.graph {
                        graph.pin(index, Some(at));
                    }
                } else if let Some((x, y)) = view.panning {
                    view.pan.0 += (f32::from(event.position.x) - x) / view.zoom;
                    view.pan.1 += (f32::from(event.position.y) - y) / view.zoom;
                    view.panning = Some((f32::from(event.position.x), f32::from(event.position.y)));
                } else {
                    view.hover = nearest_node(&hover_positions, &hover_degree, view, centre, event.position);
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |root, event: &MouseDownEvent, _, cx| {
                    let centre = down_bounds.borrow().center();
                    let view = &mut root.app.obi.graph_view;
                    match nearest_node(&down_positions, &down_degree, view, centre, event.position) {
                        Some(index) if event.click_count >= 2 => {
                            view.dragging = None;
                            let id = root.app.obi.graph.as_ref().and_then(|graph| graph.data.nodes.get(index)).map(|node| (node.id.clone(), node.kind));
                            if let Some((id, NodeKind::Note | NodeKind::Unresolved)) = id {
                                obiview::open_note(&mut root.app, &id);
                            }
                        }
                        Some(index) => view.dragging = Some(index),
                        None => view.panning = Some((f32::from(event.position.x), f32::from(event.position.y))),
                    }
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|root, _: &MouseUpEvent, _, cx| {
                    let view = &mut root.app.obi.graph_view;
                    if let (Some(index), Some(graph)) = (view.dragging.take(), &root.app.obi.graph) {
                        graph.pin(index, None);
                    }
                    view.panning = None;
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// Obiview's whole page: your notes on the left, the work in the middle, Jarvis on the right.
    pub(super) fn obi_view(&mut self, window: &mut Window, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let id = self.app.active_id();
        let wide = obiview::keeps_chat_beside(width);
        self.app.obi.narrow = !wide;
        let tab = if wide && self.app.obi.tab == Tab::Jarvis { Tab::Today } else { self.app.obi.tab };
        let tabs = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .h(px(36.))
            .px(px(10.))
            .border_b_1()
            .border_color(theme::line())
            .child(self.obi_tab("obi-tab-today", "Today", Tab::Today, cx))
            .child(self.obi_tab("obi-tab-note", "Note", Tab::Note, cx))
            .child(self.obi_tab("obi-tab-graph", "Graph", Tab::Graph, cx))
            .child(self.obi_tab("obi-tab-search", "Search", Tab::Search, cx))
            .child(self.obi_tab("obi-tab-apps", "Apps", Tab::Apps, cx))
            .when(!wide, |el| el.child(self.obi_tab("obi-tab-jarvis", "Jarvis", Tab::Jarvis, cx)));
        let page = match tab {
            Tab::Today => self.obi_today(cx),
            Tab::Note if !wide && self.app.obi.note.is_none() => self.obi_nav(cx),
            Tab::Note => self.obi_note(id, cx),
            Tab::Graph => self.obi_graph(window, cx),
            Tab::Search => self.obi_search(cx),
            Tab::Apps => self.obi_apps(cx),
            Tab::Jarvis => self.chat_body(id, cx),
        };
        let chat = if wide { self.chat_body(id, cx) } else { div().into_any_element() };
        let composer = wide.then(|| self.composer_view(cx).into_any_element());
        div()
            .flex()
            .size_full()
            .when(wide, |el| {
                let nav = self.obi_nav(cx);
                el.child(nav)
            })
            .child(div().flex().flex_col().flex_1().min_w_0().child(tabs).child(div().flex_1().min_h_0().child(page)))
            .when(wide, |el| el.child(div().flex().flex_col().flex_none().w(px(CHAT_WIDTH)).h_full().border_l_1().border_color(theme::line()).child(div().flex_1().min_h_0().child(chat)).children(composer)))
            .into_any_element()
    }
}

fn paint_graph(bounds: Bounds<Pixels>, nodes: &[(String, NodeKind, usize)], edges: &[(usize, usize)], positions: &[(f32, f32)], view: &obiview::GraphView, window: &mut Window, cx: &mut gpui::App) {
    let centre = bounds.center();
    let neighbours: Vec<usize> = view.hover.map(|hover| edges.iter().filter_map(|(a, b)| if *a == hover { Some(*b) } else if *b == hover { Some(*a) } else { None }).collect()).unwrap_or_default();
    let lit = |index: usize| view.hover.is_none_or(|hover| hover == index || neighbours.contains(&index));
    // Lines get their own layer, or GPUI's batching can draw them across the dots.
    window.paint_layer(bounds, |window| {
        for (a, b) in edges {
            let (Some(from), Some(to)) = (positions.get(*a), positions.get(*b)) else { continue };
            let mut line = PathBuilder::stroke(px(1.0));
            line.move_to(to_screen(*from, view, centre));
            line.line_to(to_screen(*to, view, centre));
            let focus = view.hover.is_none_or(|hover| hover == *a || hover == *b);
            if let Ok(path) = line.build() {
                window.paint_path(path, theme::tint(theme::line(), if focus { 1.0 } else { 0.25 }));
            }
        }
    });
    window.paint_layer(bounds, |window| {
        for (index, (label, kind, degree)) in nodes.iter().enumerate() {
            let Some(position) = positions.get(index) else { continue };
            let at = to_screen(*position, view, centre);
            if !bounds.contains(&at) {
                continue;
            }
            let radius = node_radius(*degree, 1.0) * view.zoom.sqrt();
            let focus = lit(index);
            let color = if view.hover == Some(index) { theme::accent() } else { node_color(kind, focus) };
            let square = Bounds::new(point(at.x - px(radius), at.y - px(radius)), size(px(radius * 2.0), px(radius * 2.0)));
            window.paint_quad(fill(square, color).corner_radii(px(radius)));
            let alpha = if view.hover == Some(index) { 1.0 } else { label_alpha(view.zoom, LABEL_FADE) * if focus { 1.0 } else { 0.2 } };
            if alpha > 0.02 {
                let text: SharedString = label.clone().into();
                let run = TextRun { len: text.len(), font: font(FONT), color: theme::tint(theme::text(), alpha), background_color: None, underline: None, strikethrough: None };
                let shaped = window.text_system().shape_line(text, px(12.), &[run], None);
                let origin = point(at.x - shaped.width / 2.0, at.y + px(radius + 2.0));
                let _ = shaped.paint(origin, px(16.), window, cx);
            }
        }
    });
}
