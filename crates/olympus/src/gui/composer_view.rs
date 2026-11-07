use super::chrome::model_label;
use crate::session::account_name;
use olympus_core::target::Provider;
use super::parts::word_at;
use super::{control, primary_control, Root, FONT, LINE};
use crate::input::{KeyCode, KeyEvent, KeyModifiers};
use crate::session::{Activity, Pane};
use crate::theme;
use gpui::prelude::*;
use gpui::{canvas, deferred, div, fill, font, point, px, size, Animation, AnimationExt as _, AnyElement, Bounds, Context, CursorStyle, FontWeight, MouseButton, MouseDownEvent, Pixels, Point, SharedString, StyledText, TextRun};
use std::time::Duration;

const MAX_INPUT_HEIGHT: f32 = LINE * 8.0;
const VISIBLE_HINTS: usize = 9;

impl Root {
    fn composer_index(&self, position: Point<Pixels>) -> usize {
        match &self.composer_layout {
            Some(layout) if !self.app.composer.text.is_empty() => layout.index_for_position(position).unwrap_or_else(|nearest| nearest),
            _ => 0,
        }
    }

    pub(super) fn composer_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let index = self.composer_index(position);
        self.app.composer.move_to(index, true);
        cx.notify();
    }

    fn composer_mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        self.selection = None;
        self.app.obi.searching = false;
        let index = self.composer_index(event.position);
        let composer = &mut self.app.composer;
        match event.click_count {
            1 => {
                composer.move_to(index, event.modifiers.shift);
                self.composer_dragging = true;
            }
            2 => {
                let word = word_at(&composer.text, index);
                composer.anchor = Some(word.start);
                composer.cursor = word.end;
            }
            _ => composer.select_all(),
        }
        cx.notify();
    }

    fn composer_text(&self, placeholder: &str) -> (SharedString, Vec<TextRun>) {
        let composer = &self.app.composer;
        let base = TextRun { len: 0, font: font(FONT), color: theme::text(), background_color: None, underline: None, strikethrough: None };
        if composer.text.is_empty() {
            return (placeholder.to_string().into(), vec![TextRun { len: placeholder.len(), color: theme::dim(), ..base }]);
        }
        let len = composer.text.len();
        let selection = composer.selection();
        let marked = composer.marked.clone();
        let mut cuts = vec![0, len];
        for range in selection.iter().chain(marked.iter()) {
            cuts.extend([range.start, range.end]);
        }
        cuts.sort_unstable();
        cuts.dedup();
        let runs = cuts
            .windows(2)
            .map(|pair| {
                let inside = |range: &Option<std::ops::Range<usize>>| range.as_ref().is_some_and(|r| pair[0] >= r.start && pair[1] <= r.end);
                TextRun {
                    len: pair[1] - pair[0],
                    background_color: inside(&selection).then(|| theme::tint(theme::accent(), 0.3)),
                    underline: inside(&marked).then(|| gpui::UnderlineStyle { thickness: px(1.), color: Some(theme::accent()), wavy: false }),
                    ..base.clone()
                }
            })
            .collect();
        (composer.text.clone().into(), runs)
    }

    fn attachment_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.app.attachments.is_empty() {
            return None;
        }
        let chips = self.app.attachments.iter().enumerate().map(|(index, path)| {
            let name: SharedString = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().to_string()).into();
            div()
                .id(("attachment", index))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .max_w(px(260.))
                .pl(px(4.))
                .pr(px(2.))
                .py(px(3.))
                .rounded(px(8.))
                .bg(theme::overlay())
                .when(olympus_core::agent::is_image(path), |el| el.child(div().flex_none().size(px(32.)).rounded(px(5.)).overflow_hidden().child(gpui::img(path.clone()).size_full().object_fit(gpui::ObjectFit::Cover))))
                .child(div().min_w_0().line_clamp(1).text_ellipsis().text_size(px(12.)).text_color(theme::dim()).child(name))
                .child(
                    div()
                        .id(("attachment-remove", index))
                        .flex_none()
                        .w(px(22.))
                        .text_center()
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_color(theme::dim())
                        .hover(|s| s.bg(theme::raised()).text_color(theme::text()))
                        .child("×")
                        .on_click(cx.listener(move |root, _, _, cx| {
                            if index < root.app.attachments.len() {
                                root.app.attachments.remove(index);
                            }
                            cx.notify();
                        })),
                )
        });
        Some(div().flex().flex_wrap().gap(px(6.)).children(chips).into_any_element())
    }

    fn hints(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let matches = self.app.command_matches();
        if matches.is_empty() || self.app.picker.is_some() {
            return None;
        }
        let selected = self.app.hint_selected;
        let total = matches.len();
        let start = selected.saturating_sub(VISIBLE_HINTS - 1);
        let rows = matches.into_iter().enumerate().skip(start).take(VISIBLE_HINTS).map(|(position, command)| {
            let chosen = position == selected;
            div()
                .id(("hint", position))
                .flex()
                .gap_4()
                .px(px(10.))
                .py(px(3.))
                .rounded(px(6.))
                .cursor_pointer()
                .when(chosen, |el| el.bg(theme::accent()).text_color(theme::bg()))
                .when(!chosen, |el| el.hover(|s| s.bg(theme::raised())))
                .on_click(cx.listener(move |root, _, _, cx| {
                    root.app.hint_selected = position;
                    root.app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::default()));
                    root.after_change(cx);
                }))
                .child(div().flex_none().w(px(230.)).line_clamp(1).text_ellipsis().font_weight(if chosen { FontWeight::BOLD } else { FontWeight::NORMAL }).child(SharedString::from(command.usage)))
                .child(div().flex_1().min_w_0().line_clamp(1).text_ellipsis().text_size(px(12.)).text_color(if chosen { theme::bg() } else { theme::dim() }).child(SharedString::from(command.about)))
        });
        Some(
            deferred(
                div()
                    .absolute()
                    .bottom_full()
                    .left_0()
                    .right_0()
                    .mb(px(6.))
                    .p(px(6.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .rounded(px(10.))
                    .bg(theme::overlay())
                    .border_1()
                    .border_color(theme::line())
                    .shadow_lg()
                    .children(rows)
                    .when(total > VISIBLE_HINTS, |el| el.child(div().px(px(10.)).text_size(px(12.)).text_color(theme::dim()).child(format!("{} of {total}  ·  ↑↓ choose  ·  Tab complete", selected + 1)))),
            )
            .into_any_element(),
        )
    }

    pub(super) fn composer_view(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.app.windows.get(self.app.active);
        let running = active.is_some_and(|w| w.activity == Activity::Running);
        let (model, full_profile, account, has_accounts) = match active.map(|w| &w.pane) {
            Some(Pane::Chat(chat)) => {
                let provider = chat.state.provider();
                let account = match provider {
                    Some(Provider::OpenRouter) => "OpenRouter".to_string(),
                    Some(Provider::Codex) => account_name(chat.state.codex_account_dir.as_deref()),
                    _ => account_name(chat.state.account_dir.as_deref()),
                };
                (model_label(&chat.state.choice, chat.state.last_target.as_ref()), chat.state.full_profile, account, !matches!(provider, Some(Provider::Node(_))))
            }
            _ => (String::new(), false, String::new(), false),
        };
        let placeholder = if self.app.obiview_active() { "Ask Jarvis what's going on" } else if running { "Working. Type to queue the next message." } else { "Message, /command or @model" };
        let (text, runs) = self.composer_text(placeholder);
        let styled = StyledText::new(text).with_runs(runs);
        let layout = styled.layout().clone();
        self.composer_layout = Some(layout.clone());
        let cursor = self.app.composer.cursor;
        let is_empty = self.app.composer.text.is_empty();
        let has_selection = self.app.composer.selection().is_some();
        let keyboard_here = self.app.picker.is_none();
        let root = cx.entity().downgrade();
        let scroll = self.composer_scroll.clone();
        let caret = canvas(
            move |bounds, _, _| bounds,
            move |bounds, _, window, cx| {
                let at = if is_empty { Some(bounds.origin) } else { layout.position_for_index(cursor) };
                let at = at.unwrap_or_else(|| point(bounds.left(), bounds.bottom() - px(LINE)));
                let caret = Bounds::new(at, size(px(2.), px(LINE)));
                if keyboard_here && !has_selection {
                    window.paint_quad(fill(caret, theme::accent()));
                }
                let visible = scroll.bounds();
                let offset = scroll.offset();
                let above = at.y < visible.top();
                let below = at.y + px(LINE) > visible.bottom();
                if above || below {
                    let shift = if above { visible.top() - at.y } else { visible.bottom() - (at.y + px(LINE)) };
                    scroll.set_offset(point(offset.x, offset.y + shift));
                    window.refresh();
                }
                if let Some(root) = root.upgrade() {
                    root.update(cx, |root, _| root.caret = Some(caret));
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let reduce_motion = self.app.reduce_motion;
        let jarvis = self.app.obiview_active();
        let next_profile = if full_profile { "/profile lean" } else { "/profile full" };
        let hints = self.hints(cx);
        let attachments = self.attachment_row(cx);
        let border = if running { theme::line() } else { theme::tint(theme::accent(), 0.55) };
        div().flex_none().px(px(14.)).pb(px(12.)).pt(px(6.)).child(div().relative().w_full().max_w(px(908.)).mx_auto().when(jarvis && !self.app.obi.narrow, |el| el.w(px(crate::gui::obi_view::CHAT_WIDTH - 28.))).children(hints).child(
            div()
                .id("composer")
                .relative()
                .flex()
                .flex_col()
                .gap(px(8.))
                .w_full()
                .pt(px(10.))
                .pb(px(8.))
                .px(px(12.))
                .rounded(px(12.))
                .bg(theme::raised())
                .border_1()
                .border_color(border)
                .shadow_md()
                .overflow_hidden()
                .when(running && !reduce_motion, |el| {
                    el.child(div().absolute().top_0().left_0().right_0().h(px(2.)).child(div().h_full().w(gpui::relative(0.22)).bg(theme::accent()).with_animation(
                        "activity-scan",
                        Animation::new(Duration::from_millis(1500)).repeat().with_easing(gpui::ease_in_out),
                        |el, delta| el.ml(gpui::relative(delta * 1.22 - 0.22)),
                    )))
                })
                .children(attachments)
                .child(
                    div()
                        .id("composer-input")
                        .relative()
                        .min_h(px(LINE))
                        .max_h(px(MAX_INPUT_HEIGHT))
                        .overflow_y_scroll()
                        .track_scroll(&self.composer_scroll)
                        .cursor(CursorStyle::IBeam)
                        .on_mouse_down(MouseButton::Left, cx.listener(|root, event: &MouseDownEvent, _, cx| root.composer_mouse_down(event, cx)))
                        .child(div().relative().child(styled).child(caret)),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(control("pick-model", SharedString::from(model), theme::accent()).on_click(cx.listener(|root, _, _, cx| root.run_command("/model", cx))))
                        .when(has_accounts && !jarvis, |el| {
                            el.child(control("pick-account", SharedString::from(format!("account: {account}")), theme::dim()).on_click(cx.listener(|root, _, _, cx| root.run_command("/account", cx))))
                        })
                        .when(!jarvis, |el| el.child(control("toggle-profile", if full_profile { "full setup" } else { "lean setup" }, theme::dim()).on_click(cx.listener(move |root, _, _, cx| root.run_command(next_profile, cx)))))
                        .child(if self.app.obiview_active() { div().flex_none().px(px(8.)).text_size(px(12.)).text_color(theme::dim()).child("read-only") } else { div().flex_none().px(px(8.)).text_size(px(12.)).text_color(theme::red()).child("bypass on") })
                        .child(div().flex_1())
                        .when(running, |el| {
                            el.child(control("stop", "Stop  Esc", theme::text()).border_1().border_color(theme::line()).on_click(cx.listener(|root, _, _, cx| {
                                root.app.stop_active();
                                cx.notify();
                            })))
                        })
                        .when(!running, |el| {
                            el.child(primary_control("send", "Send  Enter").on_click(cx.listener(|root, _, _, cx| {
                                root.app.submit();
                                root.after_change(cx);
                            })))
                        }),
                ),
        ))
    }
}
