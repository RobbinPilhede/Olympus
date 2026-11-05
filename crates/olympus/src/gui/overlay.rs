use super::Root;
use crate::app::Picker;
use crate::theme;
use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context, FontWeight, MouseButton, ScrollWheelEvent, SharedString};

const ROW_HEIGHT: f32 = 33.0;
const CHROME_HEIGHT: f32 = 89.0;
const TOP_GAP: f32 = 72.0;

/// How many picker rows fit in a window this tall.
pub fn rows_that_fit(window_height: f32) -> usize {
    (((window_height - TOP_GAP - 16.0 - CHROME_HEIGHT) / ROW_HEIGHT) as usize).clamp(3, 13)
}

/// The status line under a picker.
pub fn picker_footer(picker: &Picker) -> String {
    let total = picker.visible().len();
    if total == 0 {
        "Esc close".to_string()
    } else {
        format!("{} of {total}  ·  ↑↓ choose  ·  Enter open  ·  Esc close", picker.selected + 1)
    }
}

/// What a picker shows when it has no rows.
pub fn picker_message(picker: &Picker) -> Option<&str> {
    if picker.loading {
        Some("Loading…")
    } else if !picker.visible().is_empty() {
        None
    } else if picker.items.is_empty() {
        Some(&picker.empty)
    } else {
        Some("Nothing matches that filter.")
    }
}

impl Root {
    pub(super) fn picker_overlay(&mut self, window_height: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(picker) = &self.app.picker else { return div().into_any_element() };
        let visible = picker.visible();
        let visible_rows = rows_that_fit(window_height);
        let start = picker.selected.saturating_sub(visible_rows - 1);
        let rows: Vec<AnyElement> = visible
            .iter()
            .enumerate()
            .skip(start)
            .take(visible_rows)
            .map(|(position, item_index)| {
                let item = &picker.items[*item_index];
                let chosen = position == picker.selected;
                div()
                    .id(("pick", position))
                    .flex()
                    .flex_none()
                    .gap_4()
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(chosen, |el| el.bg(theme::accent()).text_color(theme::bg()))
                    .when(!chosen, |el| el.hover(|s| s.bg(theme::raised())))
                    .on_click(cx.listener(move |root, _, _, cx| {
                        root.app.choose_picker(position);
                        root.after_change(cx);
                    }))
                    .child(div().flex_none().max_w(gpui::relative(0.55)).line_clamp(1).text_ellipsis().font_weight(if chosen { FontWeight::BOLD } else { FontWeight::NORMAL }).child(SharedString::from(item.label.clone())))
                    .child(div().flex_1().min_w_0().line_clamp(1).text_ellipsis().text_size(px(12.)).text_color(if chosen { theme::bg() } else { theme::dim() }).child(SharedString::from(item.detail.clone())))
                    .into_any_element()
            })
            .collect();
        let message = picker_message(picker).map(String::from);
        let query: SharedString = picker.query.clone().into();
        let has_query = !picker.query.is_empty();
        let title: SharedString = picker.title.clone().into();
        let footer = picker_footer(picker);
        div()
            .id("picker-scrim")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(TOP_GAP))
            .px(px(16.))
            .bg(gpui::Hsla { h: 0., s: 0., l: 0., a: 0.5 })
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|root, _, _, cx| {
                    root.app.picker = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("picker")
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(680.))
                    .h(gpui::Length::Auto)
                    .rounded(px(12.))
                    .bg(theme::overlay())
                    .border_1()
                    .border_color(theme::tint(theme::accent(), 0.6))
                    .shadow_2xl()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_scroll_wheel(cx.listener(|root, event: &ScrollWheelEvent, _, cx| {
                        let lines = f32::from(event.delta.pixel_delta(px(20.)).y);
                        if let Some(picker) = &mut root.app.picker {
                            let count = picker.visible().len();
                            if lines > 0.0 {
                                picker.selected = picker.selected.saturating_sub(1);
                            } else if lines < 0.0 && picker.selected + 1 < count {
                                picker.selected += 1;
                            }
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(px(10.))
                            .h(px(44.))
                            .px(px(14.))
                            .border_b_1()
                            .border_color(theme::line())
                            .child(div().flex_none().text_color(theme::accent()).font_weight(FontWeight::BOLD).child("›"))
                            .child(div().flex().flex_1().min_w_0().items_center().child(div().line_clamp(1).text_ellipsis().text_color(if has_query { theme::text() } else { theme::dim() }).child(if has_query { query } else { "Type to filter".into() })).child(div().flex_none().w(px(2.)).h(px(18.)).bg(theme::accent())))
                            .child(div().flex_none().text_size(px(12.)).text_color(theme::dim()).child(title)),
                    )
                    .child(div().flex().flex_col().gap_1().p(px(6.)).children(rows).when_some(message, |el, message| el.child(div().p(px(10.)).text_color(theme::dim()).child(message))))
                    .child(div().flex_none().px(px(14.)).py(px(6.)).border_t_1().border_color(theme::line()).text_size(px(12.)).text_color(theme::dim()).child(footer)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Action, PickItem};

    fn item(label: &str) -> PickItem {
        PickItem { label: label.into(), detail: "first".into(), action: Action::Run("/help".into()) }
    }

    #[test]
    fn the_palette_never_asks_for_more_rows_than_fit() {
        assert_eq!(rows_that_fit(840.0), 13);
        assert_eq!(rows_that_fit(380.0), 6);
        assert_eq!(rows_that_fit(100.0), 3);
    }

    #[test]
    fn every_picker_state_explains_itself() {
        assert_eq!(picker_message(&Picker::loading("Nodes")), Some("Loading…"));
        assert_eq!(picker_message(&Picker::new("Nodes", Vec::new(), "No nodes answered.")), Some("No nodes answered."));
        let mut picker = Picker::new("Nodes", vec![item("alpha")], "");
        assert_eq!(picker_message(&picker), None);
        assert!(picker_footer(&picker).starts_with("1 of 1"));
        picker.query = "zzz".into();
        assert_eq!(picker_message(&picker), Some("Nothing matches that filter."));
        assert_eq!(picker_footer(&picker), "Esc close");
    }
}
