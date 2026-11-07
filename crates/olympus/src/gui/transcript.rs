use super::parts::{block_parts, link_target, split_for_selection, word_at, Part, PartKind, Selection, TextPos};
use super::{control, live_mark, logo, text_run, PaintedText, Root, BODY, LINE};
use crate::markdown::Rich;
use crate::session::{Pane, ToolStatus};
use crate::theme;
use gpui::prelude::*;
use gpui::{canvas, div, list, px, Animation, AnimationExt as _, AnyElement, Context, CursorStyle, FontWeight, InteractiveText, MouseButton, MouseDownEvent, Pixels, Point, ScrollWheelEvent, SharedString, StyledText, Window};
use olympus_core::workflow::TaskState;
use std::rc::Rc;
use std::time::Duration;
use windows::core::{w, HSTRING};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const COLUMN: f32 = 860.0;

// Hands a link to Windows, which opens it in whatever you've set for that kind of file.
fn open_with_windows(target: &str) -> bool {
    // SAFETY: both strings outlive the call, and no owner window is needed.
    let result = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(target), None, None, SW_SHOWNORMAL) };
    // Windows documents anything above 32 as success.
    result.0 as isize > 32
}

impl Root {
    fn hit_text(&self, window: u64, position: Point<Pixels>) -> Option<TextPos> {
        let painted = self.painted.borrow();
        let mut best: Option<(f32, &PaintedText)> = None;
        for text in painted.iter().filter(|t| t.window == window) {
            let bounds = text.bounds;
            let dy = (f32::from(bounds.top()) - f32::from(position.y)).max(f32::from(position.y) - f32::from(bounds.bottom())).max(0.0);
            let dx = (f32::from(bounds.left()) - f32::from(position.x)).max(f32::from(position.x) - f32::from(bounds.right())).max(0.0);
            // Rows matter first; within a row, table cells are told apart by column.
            let distance = dy * 10_000.0 + dx;
            if best.as_ref().is_none_or(|(closest, _)| distance < *closest) {
                best = Some((distance, text));
            }
        }
        let (_, text) = best?;
        let offset = text.layout.index_for_position(position).unwrap_or_else(|nearest| nearest);
        Some(TextPos { block: text.block, part: text.part, offset: text.base + offset })
    }

    fn part_text(&self, window: u64, pos: TextPos) -> Option<String> {
        let found = self.app.windows.iter().find(|w| w.id == window)?;
        let Pane::Chat(chat) = &found.pane else { return None };
        let parts = block_parts(chat.state.blocks.get(pos.block)?);
        Some(parts.get(pos.part)?.rich.text.clone())
    }

    fn select_start(&mut self, window: u64, event: &MouseDownEvent, cx: &mut Context<Self>) {
        self.term_selection = None;
        let Some(pos) = self.hit_text(window, event.position) else {
            self.selection = None;
            cx.notify();
            return;
        };
        let (mut anchor, mut head) = (pos, pos);
        if event.click_count >= 2 {
            if let Some(text) = self.part_text(window, pos) {
                let range = if event.click_count == 2 { word_at(&text, pos.offset) } else { 0..text.len() };
                anchor.offset = range.start;
                head.offset = range.end;
            }
        } else if event.modifiers.shift {
            if let Some(existing) = self.selection.filter(|s| s.window == window) {
                anchor = existing.anchor;
            }
        }
        self.selection = Some(Selection { window, anchor, head, dragging: event.click_count == 1 });
        cx.notify();
    }

    pub(super) fn selection_drag(&mut self, position: Point<Pixels>, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.filter(|s| s.dragging) else { return };
        if let Some(view) = self.views.get(&selection.window) {
            let viewport = view.list.viewport_bounds();
            if position.y < viewport.top() {
                self.wheel(selection.window, 24.0);
            } else if position.y > viewport.bottom() {
                self.wheel(selection.window, -24.0);
            }
        }
        if let Some(head) = self.hit_text(selection.window, position) {
            if let Some(current) = &mut self.selection {
                current.head = head;
            }
            cx.notify();
        }
    }

    fn open_link(&mut self, window: u64, target: &str) {
        if crate::obiview::is_vault_link(target) {
            crate::obiview::follow_link(&mut self.app, target);
            return;
        }
        let cwd = self.app.windows.iter().find(|w| w.id == window).and_then(|w| if let Pane::Chat(chat) = &w.pane { Some(chat.state.cwd.clone()) } else { None }).unwrap_or_default();
        let resolved = link_target(target, &cwd);
        if !open_with_windows(&resolved) {
            self.app.fail_active(format!("Unable to open {resolved}. Check that it still exists."));
        }
    }

    fn rich_el(&self, window: u64, block: usize, part: usize, data: &Part, cx: &mut Context<Self>) -> AnyElement {
        self.text_el((window, block, part), &data.rich, 0, data.rich.text.len(), cx)
    }

    fn text_el(&self, (window, block, part): (u64, usize, usize), rich: &Rich, base: usize, part_len: usize, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selection.and_then(|s| s.range_in(window, block, part, part_len)).and_then(|range| {
            let (start, end) = (range.start.max(base), range.end.min(base + rich.text.len()));
            (start < end).then(|| start - base..end - base)
        });
        let runs = split_for_selection(rich, selected.as_ref()).into_iter().map(|(len, style, in_selection)| text_run(len, &style, in_selection)).collect();
        let text = StyledText::new(SharedString::from(rich.text.clone())).with_runs(runs);
        let layout = text.layout().clone();
        let painted = self.painted.clone();
        let body = if rich.links.is_empty() {
            text.into_any_element()
        } else {
            let (ranges, targets): (Vec<_>, Vec<_>) = rich.links.iter().cloned().unzip();
            let root = cx.entity().downgrade();
            InteractiveText::new(SharedString::from(format!("text-{window}-{block}-{part}-{base}")), text)
                .on_click(ranges, move |index, _, cx| {
                    let _ = root.update(cx, |root, cx| {
                        root.open_link(window, &targets[index]);
                        cx.notify();
                    });
                })
                .into_any_element()
        };
        div()
            .relative()
            .min_w_0()
            .cursor(CursorStyle::IBeam)
            .child(body)
            .child(
                canvas(
                    move |bounds, _, _| {
                        painted.borrow_mut().push(PaintedText { window, block, part, base, layout, bounds });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .into_any_element()
    }

    fn mark_el(&self, status: Option<ToolStatus>, state: Option<TaskState>, id: SharedString) -> AnyElement {
        let running = status == Some(ToolStatus::Running) || matches!(state, Some(TaskState::Running | TaskState::Verifying | TaskState::Merging));
        if running {
            return live_mark(id, self.app.reduce_motion);
        }
        let (glyph, color) = match (status, state) {
            (Some(ToolStatus::Ok), _) | (_, Some(TaskState::Done)) => ("✓", theme::green()),
            (Some(ToolStatus::Failed), _) | (_, Some(TaskState::Failed)) => ("✗", theme::red()),
            (_, Some(TaskState::Skipped)) => ("–", theme::dim()),
            _ => ("○", theme::dim()),
        };
        div().flex_none().text_color(color).child(glyph).into_any_element()
    }

    pub(super) fn part_el(&self, window: u64, block: usize, index: usize, part: &Part, cx: &mut Context<Self>) -> AnyElement {
        let text = self.rich_el(window, block, index, part, cx);
        match &part.kind {
            PartKind::Picture { path, width, height } => {
                let target = path.display().to_string();
                div()
                    .id(SharedString::from(format!("picture-{window}-{block}-{index}")))
                    .flex_none()
                    .w(px(*width))
                    .max_w_full()
                    .h(px(*height))
                    .rounded(px(8.))
                    .overflow_hidden()
                    .cursor_pointer()
                    .border_1()
                    .border_color(theme::line())
                    .hover(|s| s.border_color(theme::tint(theme::accent(), 0.5)))
                    .on_click(cx.listener(move |root, _, _, cx| {
                        root.open_link(window, &target);
                        cx.notify();
                    }))
                    .child(gpui::img(path.clone()).size_full().object_fit(gpui::ObjectFit::Contain))
                    .into_any_element()
            }
            PartKind::User => div().flex().gap(px(10.)).px(px(12.)).py(px(8.)).rounded(px(8.)).bg(theme::raised()).child(div().flex_none().text_color(theme::accent()).font_weight(FontWeight::BOLD).child("›")).child(div().flex_1().min_w_0().child(text)).into_any_element(),
            PartKind::Label | PartKind::Phase => div().child(text).into_any_element(),
            PartKind::Para => div().child(text).into_any_element(),
            PartKind::Heading(level) => {
                let (size, line) = match level {
                    1 => (19.0, 27.0),
                    2 => (16.0, 24.0),
                    _ => (14.0, 22.0),
                };
                div().pt(px(6.)).text_size(px(size)).line_height(px(line)).font_weight(FontWeight::BOLD).child(text).into_any_element()
            }
            PartKind::Item { depth, marker } => div()
                .flex()
                .gap(px(8.))
                .pl(px(*depth as f32 * 18.0))
                .child(div().flex_none().text_color(theme::accent()).child(SharedString::from(marker.clone())))
                .child(div().flex_1().min_w_0().child(text))
                .into_any_element(),
            PartKind::Quote => div().pl(px(12.)).border_l_2().border_color(theme::line()).child(text).into_any_element(),
            PartKind::Rule => div().my(px(6.)).h(px(1.)).bg(theme::line()).into_any_element(),
            PartKind::Code { lang } => {
                let copy_text = part.rich.text.clone();
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(8.))
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::line())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .h(px(28.))
                            .pl(px(12.))
                            .pr(px(4.))
                            .text_size(px(12.))
                            .text_color(theme::dim())
                            .child(SharedString::from(lang.clone()))
                            .child(control(SharedString::from(format!("copy-{window}-{block}-{index}")), "Copy", theme::dim()).on_click(cx.listener(move |root, _, _, cx| root.copy(copy_text.clone(), cx)))),
                    )
                    .child(div().px(px(12.)).pb(px(10.)).child(text))
                    .into_any_element()
            }
            PartKind::Table { rows, columns } => {
                let copy_text = part.rich.text.clone();
                let part_len = part.rich.text.len();
                let mut base = 0;
                let mut lines: Vec<AnyElement> = Vec::with_capacity(rows.len());
                for (row_index, row) in rows.iter().enumerate() {
                    let mut cells: Vec<AnyElement> = Vec::with_capacity(row.len());
                    for (column, cell) in row.iter().enumerate() {
                        cells.push(div().flex_none().w(gpui::relative(columns.get(column).copied().unwrap_or(0.0))).pr(px(14.)).child(self.text_el((window, block, index), cell, base, part_len, cx)).into_any_element());
                        base += cell.text.len() + 1;
                    }
                    lines.push(div().flex().w_full().px(px(12.)).py(px(7.)).when(row_index > 0, |el| el.border_t_1().border_color(theme::line())).when(row_index == 0, |el| el.bg(theme::raised())).children(cells).into_any_element());
                }
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme::line())
                    .overflow_hidden()
                    .group(SharedString::from(format!("table-{window}-{block}-{index}")))
                    .children(lines)
                    .child(
                        div().absolute().top(px(3.)).right(px(4.)).child(
                            control(SharedString::from(format!("copy-{window}-{block}-{index}")), "Copy", theme::dim()).opacity(0.).group_hover(SharedString::from(format!("table-{window}-{block}-{index}")), |s| s.opacity(1.)).bg(theme::raised()).on_click(cx.listener(move |root, _, _, cx| root.copy(copy_text.clone(), cx))),
                        ),
                    )
                    .into_any_element()
            }
            PartKind::Tool { status, detail } => {
                let expanded = self.views.get(&window).is_some_and(|v| v.expanded.contains(&block));
                let has_detail = !detail.is_empty();
                let id = SharedString::from(format!("tool-{window}-{block}"));
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .id(id.clone())
                            .flex()
                            .gap(px(10.))
                            .px(px(6.))
                            .rounded(px(5.))
                            .when(has_detail, |el| {
                                el.cursor_pointer().hover(|s| s.bg(theme::raised())).on_click(cx.listener(move |root, _, _, cx| {
                                    if let Some(view) = root.views.get_mut(&window) {
                                        if !view.expanded.remove(&block) {
                                            view.expanded.insert(block);
                                        }
                                        view.list.splice(block..block + 1, 1);
                                    }
                                    cx.notify();
                                }))
                            })
                            .child(self.mark_el(Some(*status), None, format!("{id}-mark").into()))
                            .child(div().flex_1().min_w_0().when(!expanded, |el| el.h(px(LINE)).line_clamp(1).text_ellipsis()).child(text))
                            .when(has_detail, |el| el.child(div().flex_none().text_size(px(12.)).text_color(theme::dim()).child(if expanded { "Hide" } else { "Details" }))),
                    )
                    .when(expanded, |el| el.child(div().ml(px(26.)).px(px(10.)).py(px(8.)).rounded(px(6.)).bg(theme::surface()).text_size(px(12.5)).text_color(theme::dim()).child(SharedString::from(detail.clone()))))
                    .into_any_element()
            }
            PartKind::Note => div().flex().gap(px(10.)).px(px(6.)).child(div().flex_none().text_color(theme::dim()).child("·")).child(div().flex_1().min_w_0().child(text)).into_any_element(),
            PartKind::Error => div().flex().gap(px(10.)).px(px(10.)).py(px(8.)).rounded(px(8.)).bg(theme::tint(theme::red(), 0.1)).child(div().flex_none().text_color(theme::red()).child("✗")).child(div().flex_1().min_w_0().child(text)).into_any_element(),
            PartKind::Task { state } => div().flex().gap(px(10.)).child(self.mark_el(None, Some(*state), format!("task-{window}-{block}-{index}").into())).child(div().flex_1().min_w_0().child(text)).into_any_element(),
        }
    }

    fn block_el(&mut self, window: u64, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.views.get_mut(&window) else { return div().into_any_element() };
        let born = view.born.get(&index).copied();
        let last = index + 1 == view.count;
        let cached = view.parts.get(index).cloned().flatten();
        let (parts, is_tool, after_tool, is_tasks, is_assistant) = {
            let Some(Pane::Chat(chat)) = self.app.windows.iter().find(|w| w.id == window).map(|w| &w.pane) else { return div().into_any_element() };
            let Some(block) = chat.state.blocks.get(index) else { return div().into_any_element() };
            use crate::session::Block as B;
            let parts = cached.unwrap_or_else(|| Rc::new(block_parts(block)));
            let is_tool = matches!(block, B::Tool { .. });
            let after_tool = index > 0 && matches!(chat.state.blocks.get(index - 1), Some(B::Tool { .. }));
            (parts, is_tool, after_tool, matches!(block, B::Tasks { .. }), matches!(block, B::Assistant { .. }))
        };
        if let Some(slot) = self.views.get_mut(&window).and_then(|v| v.parts.get_mut(index)) {
            *slot = Some(parts.clone());
        }
        let children: Vec<AnyElement> = parts.iter().enumerate().map(|(i, part)| self.part_el(window, index, i, part, cx)).collect();
        let body = div().flex().flex_col().gap(px(if is_tool { 0. } else { 8. })).w_full().max_w(px(COLUMN)).mx_auto().relative();
        let body = if is_tasks { body.p(px(12.)).rounded(px(10.)).bg(theme::surface()).border_1().border_color(theme::line()) } else { body };
        let group = SharedString::from(format!("block-{window}-{index}"));
        let copy_text = parts.iter().map(|p| p.rich.text.as_str()).filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n\n");
        let body = body
            .group(group.clone())
            .children(children)
            .when(is_assistant && !copy_text.is_empty(), |el| {
                el.child(
                    div()
                        .absolute()
                        .top(px(-4.))
                        .right_0()
                        .opacity(0.)
                        .group_hover(group.clone(), |s| s.opacity(1.))
                        .child(control(SharedString::from(format!("copy-all-{window}-{index}")), "Copy", theme::dim()).bg(theme::bg()).on_click(cx.listener(move |root, _, _, cx| root.copy(copy_text.clone(), cx)))),
                )
            });
        // Not a flex row: its text would be measured at full width, before the column limit, and overlap the next block.
        let row = div().w_full().px(px(24.)).pt(px(if is_tool && after_tool { 2. } else { 14. })).when(last, |el| el.pb(px(20.))).child(body);
        match born.filter(|at| at.elapsed() < Duration::from_millis(240)) {
            Some(_) => row.with_animation(SharedString::from(format!("enter-{window}-{index}")), Animation::new(Duration::from_millis(200)).with_easing(gpui::ease_out_quint()), |el, delta| el.opacity(delta)).into_any_element(),
            None => row.into_any_element(),
        }
    }

    fn empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.app.obiview_active() {
            return div().p(px(18.)).text_color(theme::dim()).child("Ask Jarvis what's going on, what came in, or what's in your notes.").into_any_element();
        }
        let providers = self.app.provider_lines.iter().map(|(label, ready)| {
            div()
                .flex()
                .gap(px(10.))
                .child(div().flex_none().text_color(if *ready { theme::green() } else { theme::dim() }).child(if *ready { "✓" } else { "–" }))
                .child(div().text_color(if *ready { theme::text() } else { theme::dim() }).child(SharedString::from(label.clone())))
        });
        let examples = [
            ("/auto add login rate limiting", "Plan, build in parallel, verify, review"),
            ("@codex is this approach sound?", "Ask another model without leaving this chat"),
            ("/council which queue should we use?", "Several models, one recommendation"),
            ("/claude", "The real Claude Code, in a window"),
        ];
        let cards = examples.into_iter().enumerate().map(|(index, (example, what))| {
            div()
                .id(("example", index))
                .flex()
                .flex_col()
                .px(px(12.))
                .py(px(9.))
                .rounded(px(8.))
                .border_1()
                .border_color(theme::line())
                .cursor_pointer()
                .hover(|s| s.bg(theme::raised()).border_color(theme::tint(theme::accent(), 0.5)))
                .on_click(cx.listener(move |root, _, _, cx| {
                    root.app.composer.set(example);
                    cx.notify();
                }))
                .child(div().text_color(theme::accent()).child(example))
                .child(div().text_size(px(12.)).text_color(theme::dim()).child(what))
        });
        div()
            .id("empty")
            .size_full()
            .flex()
            .justify_center()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(22.))
                    .w_full()
                    .max_w(px(620.))
                    .px(px(24.))
                    .pt(px(72.))
                    .pb(px(24.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(logo(72.))
                            .child(div().mt(px(10.)).text_size(px(30.)).line_height(px(38.)).font_weight(FontWeight::BOLD).text_color(theme::text()).child("OLYMPUS"))
                            .child(div().text_size(px(BODY)).text_color(theme::dim()).child("Every model you're signed into, working as one team.")),
                    )
                    .child(div().flex().flex_col().gap_1().children(providers))
                    .child(div().flex().flex_col().gap(px(8.)).children(cards)),
            )
            .into_any_element()
    }

    pub(super) fn chat_body(&mut self, window: u64, cx: &mut Context<Self>) -> AnyElement {
        let empty = !self.app.windows.iter().any(|w| w.id == window && matches!(&w.pane, Pane::Chat(chat) if !chat.state.blocks.is_empty()));
        if empty {
            return self.empty_state(cx);
        }
        let Some(view) = self.views.get(&window) else { return div().into_any_element() };
        let state = view.list.clone();
        let following = state.logical_scroll_top().item_ix >= view.count;
        let weak = cx.entity().downgrade();
        let items = list(state.clone(), move |index, _, cx| match weak.upgrade() {
            Some(root) => root.update(cx, |root, cx| root.block_el(window, index, cx)),
            None => div().into_any_element(),
        })
        .size_full();
        let (wheel_state, wheel_root) = (state.clone(), cx.entity().downgrade());
        let wheel = canvas(
            |_, _, _| (),
            move |_, _, window_ref, _| {
                window_ref.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
                    if !phase.capture() || !wheel_state.viewport_bounds().contains(&event.position) {
                        return;
                    }
                    let delta = f32::from(event.delta.pixel_delta(px(LINE)).y);
                    let handled = wheel_root.upgrade().is_some_and(|root| {
                        root.update(cx, |root, cx| {
                            if root.app.picker.is_some() {
                                return false;
                            }
                            root.wheel(window, delta);
                            cx.notify();
                            true
                        })
                    });
                    if handled {
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_0();
        div()
            .relative()
            .size_full()
            .on_mouse_down(MouseButton::Left, cx.listener(move |root, event: &MouseDownEvent, _, cx| root.select_start(window, event, cx)))
            .child(items)
            .child(wheel)
            .when(!following, |el| {
                el.child(
                    div().absolute().bottom(px(10.)).right(px(18.)).child(
                        control(SharedString::from(format!("latest-{window}")), "↓ Latest", theme::accent()).bg(theme::overlay()).border_1().border_color(theme::line()).on_click(cx.listener(move |root, _, _, cx| {
                            if let Some(view) = root.views.get_mut(&window) {
                                view.glide = None;
                                let bottom = view.list.max_offset_for_scrollbar().height;
                                view.list.set_offset_from_scrollbar(gpui::point(px(0.), bottom));
                            }
                            cx.notify();
                        })),
                    ),
                )
            })
            .into_any_element()
    }
}
