//! The launch animation, shown while the installed copy updates itself or the installer runs.
use crate::theme;
use crate::update::{self, Job, Progress, Stage};
use gpui::prelude::*;
use gpui::{div, point, px, size, Animation, AnimationExt as _, App as GApp, AsyncApp, BoxShadow, Bounds, Context, FontWeight, SharedString, Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(super) const PEAK_LEFT: &str = "peak-left.svg";
pub(super) const PEAK_RIGHT: &str = "peak-right.svg";
pub(super) const BOLT: &str = "bolt.svg";
const INTRO: Duration = Duration::from_millis(1300);
const LOGO_SIZE: f32 = 104.;
const WORD: &str = "OLYMPUS";

/// Eases `t` across one part of the timeline: 0 before `start`, 1 after `end`.
fn segment(t: f32, start: f32, end: f32) -> f32 {
    let inside = ((t - start) / (end - start)).clamp(0.0, 1.0);
    1.0 - (1.0 - inside).powi(4)
}

// Rises fast as the bolt lands, then fades to reveal the bolt-shaped gap in the logo.
fn strike(t: f32) -> f32 {
    segment(t, 0.34, 0.42) * (1.0 - segment(t, 0.46, 0.8))
}

struct Splash {
    progress: Progress,
    cancel: Arc<AtomicBool>,
    intro: Duration,
}

/// Opens the splash, and calls `open_app` if the app should open in this process.
pub(super) fn open(job: Job, reduce_motion: bool, cx: &mut GApp, open_app: impl FnOnce(&mut GApp) + 'static) {
    let first = if job == Job::Install { "Installing Olympus" } else { "Checking for updates" };
    let progress: Progress = Arc::new(Mutex::new(Stage::Working(first.to_string())));
    let cancel = Arc::new(AtomicBool::new(false));
    let intro = if reduce_motion { Duration::from_millis(1) } else { INTRO };
    update::start(job, progress.clone(), cancel.clone());
    let options = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(440.), px(320.)), cx))),
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Opaque,
        app_id: Some("olympus".into()),
        ..WindowOptions::default()
    };
    let view = Splash { progress: progress.clone(), cancel, intro };
    let handle = cx.open_window(options, |_, cx| cx.new(|_| view)).expect("opening the splash");
    cx.activate(true);
    cx.spawn(async move |cx: &mut AsyncApp| {
        let started = Instant::now();
        let mut open_app = Some(open_app);
        loop {
            cx.background_executor().timer(Duration::from_millis(33)).await;
            let _ = handle.update(cx, |_, _, cx| cx.notify());
            let stage = progress.lock().map(|stage| stage.clone()).unwrap_or(Stage::OpenApp);
            if started.elapsed() < intro {
                continue;
            }
            match stage {
                Stage::OpenApp => {
                    let Some(open_app) = open_app.take() else { break };
                    let _ = cx.update(open_app);
                    let _ = handle.update(cx, |_, window, _| window.remove_window());
                    break;
                }
                Stage::Relaunch { exe, welcome } => update::relaunch(&exe, &welcome),
                _ => {}
            }
        }
    })
    .detach();
}

impl Render for Splash {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let stage = self.progress.lock().map(|stage| stage.clone()).unwrap_or(Stage::OpenApp);
        let intro = self.intro;
        let timeline = move || Animation::new(intro);
        let peak = |id: &'static str, path: &'static str, from: f32, start: f32| {
            div().absolute().top_0().left_0().size(px(LOGO_SIZE)).child(gpui::svg().path(path).size_full().text_color(theme::text())).with_animation(id, timeline(), move |el, t| {
                let shown = segment(t, start, start + 0.36);
                el.left(px(from * (1.0 - shown))).opacity(shown)
            })
        };
        let bolt = div().absolute().top_0().left_0().size(px(LOGO_SIZE)).child(gpui::svg().path(BOLT).size_full().text_color(theme::accent())).with_animation("bolt", timeline(), |el, t| el.top(px(-14. * (1.0 - segment(t, 0.34, 0.42)))).opacity(strike(t)));
        let glow = div().absolute().top(px(LOGO_SIZE * 0.45)).left(px(LOGO_SIZE * 0.45)).size(px(LOGO_SIZE * 0.1)).rounded_full().with_animation("glow", timeline(), |el, t| {
            el.shadow(vec![BoxShadow { color: theme::tint(theme::accent(), 0.5 * strike(t)), offset: point(px(0.), px(0.)), blur_radius: px(70.), spread_radius: px(26.) }])
        });
        let letters = WORD.chars().enumerate().map(|(index, letter)| {
            let start = 0.5 + index as f32 * 0.045;
            div().relative().child(SharedString::from(letter.to_string())).with_animation(("letter", index), timeline(), move |el, t| {
                let shown = segment(t, start, start + 0.3);
                el.top(px(8. * (1.0 - shown))).opacity(shown)
            })
        });
        let (status, fraction, skippable, failed) = match &stage {
            Stage::Working(text) => (text.clone(), None, false, false),
            Stage::Downloading { version, fraction } => (format!("Updating to {version} · {:.0}%", fraction * 100.0), Some(*fraction), true, false),
            Stage::Failed(text) => (text.clone(), None, false, true),
            Stage::OpenApp | Stage::Relaunch { .. } => (String::new(), None, false, false),
        };
        let busy = !status.is_empty() && !failed;
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(18.))
            .px(px(28.))
            .bg(theme::bg())
            .text_color(theme::text())
            .font_family("JetBrains Mono")
            .child(div().relative().flex_none().size(px(LOGO_SIZE)).child(glow).child(peak("peak-left", PEAK_LEFT, -24., 0.0)).child(peak("peak-right", PEAK_RIGHT, 24., 0.06)).child(bolt))
            .child(div().flex().gap(px(5.)).text_size(px(24.)).font_weight(FontWeight::BOLD).children(letters))
            .child(
                div()
                    .absolute()
                    .bottom(px(18.))
                    .left_0()
                    .right_0()
                    .px(px(28.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .child(div().h(px(18.)).max_w_full().text_size(px(12.)).text_color(if failed { theme::red() } else { theme::dim() }).child(SharedString::from(status)))
                    .child(div().relative().w(px(180.)).h(px(2.)).rounded_full().overflow_hidden().when(busy, |el| el.bg(theme::raised())).when(busy, |el| {
                        el.child(match fraction {
                            Some(fraction) => div().h_full().rounded_full().bg(theme::accent()).w(gpui::relative(fraction)).into_any_element(),
                            None => div().absolute().top_0().h_full().w(px(54.)).rounded_full().bg(theme::accent()).with_animation("busy", Animation::new(Duration::from_millis(1100)).repeat().with_easing(gpui::ease_in_out), |el, t| el.left(px(-54. + 234. * t))).into_any_element(),
                        })
                    }))
                    .child(div().h(px(26.)).when(skippable || failed, |el| {
                        let label = if failed { "Close" } else { "Skip this update" };
                        let cancel = self.cancel.clone();
                        el.child(div().id("splash-action").px(px(10.)).py(px(4.)).rounded(px(6.)).cursor_pointer().text_size(px(12.)).text_color(theme::dim()).hover(|style| style.bg(theme::raised()).text_color(theme::text())).child(label).on_click(cx.listener(move |_, _, _, _| {
                            if failed {
                                std::process::exit(1);
                            }
                            cancel.store(true, Ordering::Relaxed);
                        })))
                    }))
                    .with_animation("status", timeline(), |el, t| el.opacity(segment(t, 0.8, 1.0))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_holds_before_and_after_its_part_of_the_timeline() {
        assert!(segment(0.1, 0.2, 0.4).abs() < f32::EPSILON);
        assert!((segment(0.5, 0.2, 0.4) - 1.0).abs() < f32::EPSILON);
        assert!(segment(0.3, 0.2, 0.4) > 0.5, "it eases out, so it's past halfway by the midpoint");
    }

    #[test]
    fn the_strike_starts_and_ends_dark_and_is_bright_in_between() {
        assert!(strike(0.0).abs() < f32::EPSILON);
        assert!(strike(1.0).abs() < f32::EPSILON);
        assert!(strike(0.44) > 0.9);
    }
}
