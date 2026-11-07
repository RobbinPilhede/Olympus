use super::{control, primary_control, Root};
use crate::theme;
use gpui::prelude::*;
use gpui::{canvas, div, fill, point, px, size, Bounds, Context, FontWeight, MouseButton, SharedString};

const MODULE: f32 = 5.0;
const QUIET: f32 = 4.0;

// One matrix per panel draw is fine, since this only renders while the panel is open.
fn qr_modules(text: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let dark = code.to_colors().iter().map(|color| *color == qrcode::Color::Dark).collect();
    Some((code.width(), dark))
}

fn qr_view(text: &str) -> gpui::AnyElement {
    let Some((width, dark)) = qr_modules(text) else { return div().child("The address is too long for a QR code.").into_any_element() };
    let side = (width as f32 + QUIET * 2.0) * MODULE;
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            // A QR code has to be dark on light whatever the theme, or scanners can't read it.
            window.paint_quad(fill(bounds, theme::accent()));
            for (index, _) in dark.iter().enumerate().filter(|(_, is_dark)| **is_dark) {
                let (column, row) = (index % width, index / width);
                let origin = point(bounds.left() + px((column as f32 + QUIET) * MODULE), bounds.top() + px((row as f32 + QUIET) * MODULE));
                window.paint_quad(fill(Bounds::new(origin, size(px(MODULE), px(MODULE))), theme::bg()));
            }
        },
    )
    .flex_none()
    .size(px(side))
    .into_any_element()
}

impl Root {
    /// The always-visible switch for phone access, at the bottom left.
    pub(super) fn mirror_switch(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.app.mirror.is_some();
        let label: SharedString = self.app.mirror.as_ref().map_or_else(|| "Phone access".to_string(), |mirror| mirror.address().trim_start_matches("http://").to_string()).into();
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .h(px(24.))
            .child(
                div()
                    .id("mirror-details")
                    .flex()
                    .min_w_0()
                    .items_center()
                    .h_full()
                    .px(px(8.))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .text_color(if on { theme::accent() } else { theme::dim() })
                    .when(on, |el| el.cursor_pointer().hover(|s| s.bg(theme::overlay())))
                    .when(on, |el| {
                        el.on_click(cx.listener(|root, _, _, cx| {
                            root.app.mirror_panel = true;
                            cx.notify();
                        }))
                    })
                    .child(div().line_clamp(1).text_ellipsis().child(label)),
            )
            .child(
                div()
                    .id("mirror-switch")
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
                    .on_click(cx.listener(|root, _, _, cx| {
                        crate::mirror::toggle(&mut root.app);
                        root.after_change(cx);
                    }))
                    .child(div().size(px(14.)).rounded_full().bg(if on { theme::bg() } else { theme::text() })),
            )
    }

    /// How to pair a phone.
    pub(super) fn mirror_panel(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(mirror) = &self.app.mirror else { return div().into_any_element() };
        let url = mirror.pairing_url();
        let address = mirror.address();
        let paired = mirror.door.lock().map(|door| door.devices.len()).unwrap_or(0);
        let code: SharedString = url.rsplit('=').next().unwrap_or_default().to_string().into();
        let paired_line: SharedString = match paired {
            0 => "No phones paired yet.".into(),
            1 => "1 device paired. It stays paired until you forget it.".into(),
            count => format!("{count} devices paired. They stay paired until you forget them.").into(),
        };
        div()
            .id("mirror-scrim")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(60.))
            .px(px(16.))
            .bg(gpui::Hsla { h: 0., s: 0., l: 0., a: 0.5 })
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|root, _, _, cx| {
                    root.app.mirror_panel = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("mirror-panel")
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(14.))
                    .w_full()
                    .max_w(px(460.))
                    .p(px(20.))
                    .rounded(px(12.))
                    .bg(theme::overlay())
                    .border_1()
                    .border_color(theme::tint(theme::accent(), 0.6))
                    .shadow_2xl()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().font_weight(FontWeight::BOLD).text_size(px(16.)).child("Open Olympus on your phone"))
                    .child(div().text_color(theme::dim()).text_size(px(12.)).child("Scan this with your phone on the same network."))
                    .child(qr_view(&url))
                    .child(div().text_size(px(12.)).text_color(theme::dim()).child(SharedString::from(format!("Or open {address} and enter"))))
                    .child(div().font_weight(FontWeight::BOLD).text_size(px(22.)).child(code))
                    .child(div().text_size(px(12.)).text_color(theme::dim()).child(paired_line))
                    .child(div().w(px(420.)).text_size(px(12.)).text_color(theme::red()).child("A paired phone can run commands on this PC. The connection isn't encrypted, so use a network you trust."))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(control("mirror-copy", "Copy address", theme::dim()).on_click(cx.listener(move |root, _, _, cx| root.copy(address.clone(), cx))))
                            .child(control("mirror-forget", "Forget all devices", theme::dim()).on_click(cx.listener(|root, _, _, cx| {
                                crate::mirror::forget(&mut root.app);
                                root.after_change(cx);
                            })))
                            .child(primary_control("mirror-done", "Done").on_click(cx.listener(|root, _, _, cx| {
                                root.app.mirror_panel = false;
                                cx.notify();
                            }))),
                    ),
            )
            .into_any_element()
    }
}
