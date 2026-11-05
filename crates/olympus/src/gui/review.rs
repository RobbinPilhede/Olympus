use super::{control, primary_control, Root};
use crate::diff::{DiffSource, DiffState, FileDiff, LineKind};
use crate::session::Pane;
use crate::theme;
use gpui::prelude::*;
use gpui::{div, px, uniform_list, Animation, AnimationExt as _, AnyElement, Context, FontWeight, SharedString};
use std::time::Duration;

const PANEL_WIDTH: f32 = 480.0;
const ROW: f32 = 20.0;

/// One row of the flattened review list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    File(usize),
    Line(usize, usize),
    Binary(usize),
}

/// Flattens files into rows, leaving out collapsed files' lines.
pub fn flatten(state: &DiffState) -> Vec<Row> {
    let mut rows = Vec::new();
    for (index, file) in state.files.iter().enumerate() {
        rows.push(Row::File(index));
        if state.collapsed.contains(&file.path) {
            continue;
        }
        if file.binary {
            rows.push(Row::Binary(index));
        }
        rows.extend((0..file.lines.len()).map(|line| Row::Line(index, line)));
    }
    rows
}

/// The row with the most text, which sets how far the list scrolls sideways.
fn widest_row(state: &DiffState) -> Option<usize> {
    let width = |row: &Row| match *row {
        Row::Line(file, line) => state.files[file].lines[line].text.chars().count(),
        _ => 0,
    };
    flatten(state).iter().enumerate().max_by_key(|(_, row)| width(row)).map(|(index, _)| index)
}

fn number(value: Option<u32>) -> SharedString {
    value.map_or_else(SharedString::default, |n| n.to_string().into())
}

fn line_row(file: &FileDiff, line: usize) -> AnyElement {
    let line = &file.lines[line];
    let (sign, color, background) = match line.kind {
        LineKind::Add => ("+", theme::text(), Some(theme::tint(theme::green(), 0.13))),
        LineKind::Remove => ("-", theme::text(), Some(theme::tint(theme::red(), 0.13))),
        LineKind::Hunk => ("", theme::bright(), Some(theme::surface())),
        LineKind::Context => (" ", theme::dim(), None),
    };
    let sign_color = match line.kind {
        LineKind::Add => theme::green(),
        LineKind::Remove => theme::red(),
        _ => theme::dim(),
    };
    let text: SharedString = line.text.replace('\t', "    ").into();
    div()
        .flex()
        .h(px(ROW))
        .w_full()
        .pr(px(8.))
        .text_size(px(12.))
        .line_height(px(ROW))
        .text_color(color)
        .when_some(background, |el, background| el.bg(background))
        .child(div().flex_none().w(px(38.)).text_right().pr(px(6.)).text_color(theme::dim()).child(number(line.old)))
        .child(div().flex_none().w(px(38.)).text_right().pr(px(6.)).text_color(theme::dim()).child(number(line.new)))
        .child(div().flex_none().w(px(14.)).text_color(sign_color).child(sign))
        .child(div().flex_none().whitespace_nowrap().child(text))
        .into_any_element()
}

impl Root {
    fn file_row(&self, id: u64, file: &FileDiff, collapsed: bool, cx: &mut Context<Self>) -> AnyElement {
        let path = file.path.clone();
        let label: SharedString = file.path.clone().into();
        let element_id = SharedString::from(format!("file-{id}-{path}"));
        div()
            .id(element_id)
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(ROW))
            .w_full()
            .px(px(10.))
            .bg(theme::raised())
            .text_size(px(12.))
            .line_height(px(ROW))
            .cursor_pointer()
            .hover(|s| s.bg(theme::overlay()))
            .on_click(cx.listener(move |root, _, _, cx| {
                let state = root.app.diffs.entry(id).or_default();
                if !state.collapsed.remove(&path) {
                    state.collapsed.insert(path.clone());
                }
                cx.notify();
            }))
            .child(div().flex_none().w(px(10.)).text_color(theme::dim()).child(if collapsed { "▸" } else { "▾" }))
            .child(div().flex_1().min_w_0().line_clamp(1).text_ellipsis().font_weight(FontWeight::MEDIUM).child(label))
            .child(div().flex_none().text_color(theme::green()).child(format!("+{}", file.added)))
            .child(div().flex_none().text_color(theme::red()).child(format!("-{}", file.removed)))
            .into_any_element()
    }

    fn review_rows(&mut self, id: u64, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(state) = self.app.diffs.get(&id).cloned() else { return Vec::new() };
        let rows = flatten(&state);
        rows[range.start.min(rows.len())..range.end.min(rows.len())]
            .iter()
            .map(|row| match *row {
                Row::File(index) => self.file_row(id, &state.files[index], state.collapsed.contains(&state.files[index].path), cx),
                Row::Line(file, line) => line_row(&state.files[file], line),
                Row::Binary(_) => div().h(px(ROW)).px(px(10.)).text_size(px(12.)).text_color(theme::dim()).child("Binary or too large to show").into_any_element(),
            })
            .collect()
    }

    fn review_message(&self, title: &str, detail: String) -> AnyElement {
        div().flex().flex_col().gap_1().p(px(16.)).child(div().font_weight(FontWeight::MEDIUM).child(title.to_string())).child(div().text_size(px(12.)).text_color(theme::dim()).child(detail)).into_any_element()
    }

    pub(super) fn review_panel(&mut self, full_width: bool, cx: &mut Context<Self>) -> AnyElement {
        let id = self.app.active_id();
        let branch = match self.app.windows.get(self.app.active).map(|w| &w.pane) {
            Some(Pane::Chat(chat)) => chat.state.last_branch.clone(),
            _ => None,
        };
        let state = self.app.diffs.entry(id).or_default().clone();
        let on_branch = matches!(state.source, DiffSource::Branch(_));
        let count = flatten(&state).len();
        let (added, removed) = state.files.iter().fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
        let summary = match state.files.len() {
            0 => String::new(),
            1 => format!("1 file  +{added} -{removed}"),
            n => format!("{n} files  +{added} -{removed}"),
        };
        let body: AnyElement = if let Some(error) = &state.error {
            if error.contains("git init") {
                self.review_message("This folder isn't a git repository", "Run git init here to review changes.".into())
            } else {
                self.review_message("Unable to load changes", error.clone())
            }
        } else if !state.loaded {
            self.review_message("Loading changes…", String::new())
        } else if state.files.is_empty() {
            let detail = if on_branch { "The finished run didn't change anything." } else { "Edits from this chat will appear here as they happen." };
            self.review_message("No changes yet", detail.into())
        } else {
            let weak = cx.entity().downgrade();
            uniform_list("review-rows", count, move |range, _, cx| match weak.upgrade() {
                Some(root) => root.update(cx, |root, cx| root.review_rows(id, range, cx)),
                None => Vec::new(),
            })
            .track_scroll(self.review_scroll.clone())
            .with_horizontal_sizing_behavior(gpui::ListHorizontalSizingBehavior::Unconstrained)
            .with_width_from_item(widest_row(&state))
            .flex_1()
            .min_h_0()
            .into_any_element()
        };
        let header = div()
            .flex()
            .flex_none()
            .flex_col()
            .gap_1()
            .p(px(8.))
            .border_b_1()
            .border_color(theme::line())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(control("review-working", "Working tree", if on_branch { theme::dim() } else { theme::accent() }).on_click(cx.listener(|root, _, _, cx| {
                        root.app.set_diff_source(DiffSource::Working);
                        cx.notify();
                    })))
                    .when_some(branch.clone(), |el, branch| {
                        el.child(control("review-branch", "Last /auto run", if on_branch { theme::accent() } else { theme::dim() }).on_click(cx.listener(move |root, _, _, cx| {
                            root.app.set_diff_source(DiffSource::Branch(branch.clone()));
                            cx.notify();
                        })))
                    })
                    .child(div().flex_1())
                    .child(control("review-refresh", "Refresh", theme::dim()).on_click(cx.listener(|root, _, _, cx| {
                        root.app.refresh_diff();
                        cx.notify();
                    }))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .pl(px(8.))
                    .child(div().text_size(px(12.)).text_color(theme::dim()).whitespace_nowrap().child(summary))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .when(!on_branch && !state.files.is_empty(), |el| el.child(control("review-run", "Review", theme::text()).border_1().border_color(theme::line()).on_click(cx.listener(|root, _, _, cx| root.run_command("/review", cx)))))
                            .when(on_branch && !state.files.is_empty(), |el| {
                                el.child(primary_control("review-merge", "Merge").on_click(cx.listener(|root, _, _, cx| root.run_command("/merge", cx))))
                            }),
                    ),
            );
        let panel = div().flex().flex_col().flex_none().h_full().bg(theme::bg()).border_l_1().border_color(theme::line()).overflow_hidden().child(header).child(div().flex().flex_col().flex_1().min_h_0().child(body));
        if full_width {
            return panel.flex_1().into_any_element();
        }
        if self.app.reduce_motion {
            return panel.w(px(PANEL_WIDTH)).into_any_element();
        }
        panel.with_animation("review-open", Animation::new(Duration::from_millis(170)).with_easing(gpui::ease_out_quint()), |el, delta| el.w(px(PANEL_WIDTH * delta))).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse;

    #[test]
    fn collapsing_a_file_hides_only_its_lines() {
        let mut state = DiffState { files: parse("diff --git a/a b/a\n@@ -1 +1 @@\n-x\n+y\ndiff --git a/b b/b\n@@ -1 +1 @@\n-p\n+q\n"), ..DiffState::default() };
        assert_eq!(flatten(&state).len(), 8);
        state.collapsed.insert("a".into());
        assert_eq!(flatten(&state)[..2], [Row::File(0), Row::File(1)]);
        assert_eq!(flatten(&state).len(), 5);
    }

    #[test]
    fn the_longest_line_sets_the_sideways_scroll() {
        let state = DiffState { files: parse("diff --git a/a b/a\n@@ -1,2 +1,2 @@\n-x\n+a much longer line\n"), ..DiffState::default() };
        assert_eq!(widest_row(&state), Some(3));
    }

    #[test]
    fn binary_files_get_a_row_that_says_so() {
        let state = DiffState { files: parse("diff --git a/l.png b/l.png\nBinary files a/l.png and b/l.png differ\n"), ..DiffState::default() };
        assert_eq!(flatten(&state), [Row::File(0), Row::Binary(0)]);
    }
}
