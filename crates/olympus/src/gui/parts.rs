use crate::markdown::{self, Inline, Md, Rich, Tone};
use crate::session::{Block, TaskRow, ToolStatus};
use olympus_core::agent::{attached_images, is_image, ATTACHED};
use olympus_core::workflow::TaskState;
use std::ops::Range;
use std::path::{Path, PathBuf};

const PICTURE_MAX_WIDTH: f32 = 640.;
const PICTURE_MAX_HEIGHT: f32 = 420.;

/// How a piece of a transcript block is drawn.
#[derive(Debug, Clone, PartialEq)]
pub enum PartKind {
    User,
    Label,
    Para,
    Heading(u8),
    Item { depth: usize, marker: String },
    Quote,
    Code { lang: String },
    Table { rows: Vec<Vec<Rich>>, columns: Vec<f32> },
    Rule,
    Tool { status: ToolStatus, detail: String },
    Note,
    Error,
    Phase,
    Task { state: TaskState },
    Picture { path: PathBuf, width: f32, height: f32 },
}

/// One selectable stretch of a transcript block.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub kind: PartKind,
    pub rich: Rich,
}

fn styled(tone: Tone) -> Inline {
    Inline { tone, ..Inline::default() }
}

fn part(kind: PartKind, text: impl Into<String>, style: Inline) -> Part {
    Part { kind, rich: Rich::plain(text, style) }
}

/// A task's status in words, so state never depends on color alone.
pub fn state_word(state: TaskState) -> &'static str {
    match state {
        TaskState::Pending => "waiting",
        TaskState::Running => "working",
        TaskState::Verifying => "checking",
        TaskState::Merging => "merging",
        TaskState::Done => "done",
        TaskState::Failed => "failed",
        TaskState::Skipped => "skipped",
    }
}

fn task_part(row: &TaskRow) -> Part {
    let mut rich = Rich::default();
    rich.push(&row.title, Inline::default());
    rich.push(&format!("  {}", state_word(row.state)), styled(Tone::Dim));
    if let Some(target) = &row.target {
        rich.push(&format!("  {target}"), styled(Tone::Accent));
    }
    if let Some(note) = row.note.lines().next().filter(|n| !n.is_empty()) {
        rich.push(&format!("  {note}"), styled(Tone::Dim));
    }
    Part { kind: PartKind::Task { state: row.state }, rich }
}

/// How much of the width each column gets; short columns stay short and long text shares the rest.
pub fn column_shares(rows: &[Vec<Rich>]) -> Vec<f32> {
    let count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let wanted: Vec<f32> = (0..count).map(|c| rows.iter().filter_map(|row| row.get(c)).map(|cell| cell.text.chars().count()).max().unwrap_or(0).clamp(6, 44) as f32).collect();
    let total: f32 = wanted.iter().sum();
    wanted.iter().map(|w| w / total.max(1.0)).collect()
}

// The part's text is tab-separated, so selecting or copying a table pastes cleanly into a spreadsheet.
fn table_part(mut rows: Vec<Vec<Rich>>) -> Part {
    if let Some(head) = rows.first_mut() {
        head.iter_mut().flat_map(|cell| cell.spans.iter_mut()).for_each(|span| span.1.bold = true);
    }
    let mut rich = Rich::default();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            rich.push("\n", Inline::default());
        }
        for (column, cell) in row.iter().enumerate() {
            if column > 0 {
                rich.push("\t", Inline::default());
            }
            rich.push(&cell.text, Inline::default());
        }
    }
    let columns = column_shares(&rows);
    Part { kind: PartKind::Table { rows, columns }, rich }
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = (bytes[at] == b'%').then(|| text.get(at + 1..at + 3).and_then(|pair| u8::from_str_radix(pair, 16).ok())).flatten();
        match hex {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn without_file_scheme(target: &str) -> String {
    let plain = target.strip_prefix("file:///").or_else(|| target.strip_prefix("file://")).map_or_else(|| target.to_string(), percent_decoded);
    if plain.contains('%') && !Path::new(&plain).exists() { percent_decoded(&plain) } else { plain }
}

/// What to open for a link: web addresses as they are, anything else as a file, relative to the chat's folder.
pub fn link_target(target: &str, cwd: &Path) -> String {
    let is_web = ["http://", "https://", "mailto:"].iter().any(|scheme| target.starts_with(scheme));
    if is_web {
        return target.to_string();
    }
    let path = cwd.join(without_file_scheme(target));
    // Models often cite code as `src/app.rs:12`.
    match path.to_string_lossy().rsplit_once(':') {
        Some((file, line)) if line.parse::<u32>().is_ok() => file.to_string(),
        _ => path.to_string_lossy().into_owned(),
    }
}

fn picture_size(width: u32, height: u32) -> (f32, f32) {
    let (width, height) = (width.max(1) as f32, height.max(1) as f32);
    let scale = (PICTURE_MAX_WIDTH / width).min(PICTURE_MAX_HEIGHT / height).min(1.0);
    (width * scale, height * scale)
}

fn picture_part(path: PathBuf) -> Option<Part> {
    let (width, height) = image::image_dimensions(&path).ok()?;
    let (width, height) = picture_size(width, height);
    Some(Part { kind: PartKind::Picture { path, width, height }, rich: Rich::default() })
}

fn local_picture(target: &str) -> Option<PathBuf> {
    let path = PathBuf::from(without_file_scheme(target));
    (path.is_absolute() && is_image(&path)).then_some(path)
}

// Pictures a stretch of an answer points at, by link or by writing out the path.
fn pictures_in(rich: &Rich) -> Vec<PathBuf> {
    let linked = rich.links.iter().filter_map(|(_, target)| local_picture(target));
    let written = rich.text.split(|c: char| c.is_whitespace() || "`'\"()[]<>,;*".contains(c)).filter_map(|word| local_picture(word.trim_end_matches(['.', ':'])));
    linked.chain(written).collect()
}

fn part_pictures(part: &Part) -> Vec<PathBuf> {
    match &part.kind {
        PartKind::Table { rows, .. } => rows.iter().flatten().flat_map(pictures_in).collect(),
        _ => pictures_in(&part.rich),
    }
}

fn user_parts(text: &str) -> Vec<Part> {
    let shown: Vec<&str> = text.lines().filter(|line| !line.strip_prefix(ATTACHED).is_some_and(|path| is_image(Path::new(path)))).collect();
    let mut parts = vec![part(PartKind::User, shown.join("\n").trim_end(), Inline { bold: true, ..Inline::default() })];
    parts.extend(attached_images(text).into_iter().filter_map(picture_part));
    parts
}

/// Splits a block into what the transcript draws and what selection can copy.
pub fn block_parts(block: &Block) -> Vec<Part> {
    match block {
        Block::User(text) => user_parts(text),
        Block::Assistant { label, text } => {
            let mut parts = Vec::new();
            if let Some(label) = label {
                parts.push(part(PartKind::Label, label.as_str(), Inline { bold: true, tone: Tone::Accent, ..Inline::default() }));
            }
            let mut shown: Vec<PathBuf> = Vec::new();
            for answer_part in markdown::parse(text).into_iter().map(|md| match md {
                Md::Para(rich) => Part { kind: PartKind::Para, rich },
                Md::Heading { level, mut text } => {
                    text.spans.iter_mut().for_each(|span| span.1.bold = true);
                    Part { kind: PartKind::Heading(level), rich: text }
                }
                Md::Item { depth, marker, text } => Part { kind: PartKind::Item { depth, marker }, rich: text },
                Md::Quote(rich) => Part { kind: PartKind::Quote, rich },
                Md::Code { lang, text } => part(PartKind::Code { lang }, text, Inline::default()),
                Md::Table(rows) => table_part(rows),
                Md::Rule => part(PartKind::Rule, "", Inline::default()),
            }) {
                let pictures = part_pictures(&answer_part);
                parts.push(answer_part);
                for path in pictures {
                    if !shown.contains(&path) {
                        shown.push(path.clone());
                        parts.extend(picture_part(path));
                    }
                }
            }
            parts
        }
        Block::Tool { name, detail, status, summary, .. } => {
            let mut rich = Rich::default();
            rich.push(name, styled(Tone::Bright));
            let first = detail.lines().next().unwrap_or("");
            if !first.is_empty() {
                rich.push(&format!("  {first}"), styled(Tone::Dim));
            }
            if *status == ToolStatus::Failed && !summary.is_empty() {
                rich.push(&format!("  {}", summary.lines().next().unwrap_or("")), styled(Tone::Red));
            }
            let extra = match (detail.contains('\n'), summary.is_empty()) {
                (true, true) => detail.clone(),
                (true, false) => format!("{detail}\n\n{summary}"),
                (false, false) => summary.clone(),
                (false, true) => String::new(),
            };
            vec![Part { kind: PartKind::Tool { status: *status, detail: extra }, rich }]
        }
        Block::Note(text) => vec![part(PartKind::Note, text.trim(), styled(Tone::Dim))],
        Block::Error(text) => vec![part(PartKind::Error, text.trim(), styled(Tone::Red))],
        Block::Tasks { phase, rows } => {
            let mut parts = vec![part(PartKind::Phase, phase.as_str(), Inline { bold: true, tone: Tone::Accent, ..Inline::default() })];
            parts.extend(rows.iter().map(task_part));
            parts
        }
    }
}

/// A place in a transcript's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    pub block: usize,
    pub part: usize,
    pub offset: usize,
}

/// A drag selection in one chat's transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub window: u64,
    pub anchor: TextPos,
    pub head: TextPos,
    pub dragging: bool,
}

impl Selection {
    fn ordered(&self) -> (TextPos, TextPos) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    /// True when the anchor and head differ.
    pub fn has_text(&self) -> bool {
        self.anchor != self.head
    }

    /// The selected byte range within one part, if any.
    pub fn range_in(&self, window: u64, block: usize, part: usize, len: usize) -> Option<Range<usize>> {
        if window != self.window || !self.has_text() {
            return None;
        }
        let (start, end) = self.ordered();
        let here = (block, part);
        if here < (start.block, start.part) || here > (end.block, end.part) {
            return None;
        }
        let from = if here == (start.block, start.part) { start.offset } else { 0 };
        let to = if here == (end.block, end.part) { end.offset } else { len };
        (from < to).then_some(from..to.min(len))
    }

    /// The selected text, one line per part.
    pub fn text(&self, blocks: &[Block]) -> String {
        let (start, end) = self.ordered();
        let mut out: Vec<String> = Vec::new();
        for (block_index, block) in blocks.iter().enumerate().take(end.block + 1).skip(start.block) {
            for (part_index, part) in block_parts(block).iter().enumerate() {
                if let Some(range) = self.range_in(self.window, block_index, part_index, part.rich.text.len()) {
                    if let Some(text) = part.rich.text.get(range) {
                        out.push(text.to_string());
                    }
                }
            }
        }
        out.join("\n")
    }
}

/// Styled stretches of a part, cut where the selection starts and ends.
pub fn split_for_selection(rich: &Rich, selected: Option<&Range<usize>>) -> Vec<(usize, Inline, bool)> {
    let mut out = Vec::new();
    let mut at = 0;
    for (len, style) in &rich.spans {
        let end = at + len;
        let mut cuts = vec![at, end];
        if let Some(range) = selected {
            cuts.extend([range.start, range.end].into_iter().filter(|c| *c > at && *c < end));
        }
        cuts.sort_unstable();
        for pair in cuts.windows(2) {
            let in_selection = selected.is_some_and(|r| pair[0] >= r.start && pair[1] <= r.end);
            out.push((pair[1] - pair[0], *style, in_selection));
        }
        at = end;
    }
    out
}

/// The word around a byte offset.
pub fn word_at(text: &str, offset: usize) -> Range<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let offset = offset.min(text.len());
    let start = text[..offset].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(offset, |(i, _)| i);
    let end = text[offset..].char_indices().find(|(_, c)| !is_word(*c)).map_or(text.len(), |(i, _)| offset + i);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(block: usize, part: usize, offset: usize) -> TextPos {
        TextPos { block, part, offset }
    }

    fn selection(anchor: TextPos, head: TextPos) -> Selection {
        Selection { window: 1, anchor, head, dragging: false }
    }

    fn saved_picture(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        crate::capture::save_png(&path, 1280, 720, &vec![9; 1280 * 720 * 4]).expect("saved");
        path
    }

    fn picture_paths(parts: &[Part]) -> Vec<PathBuf> {
        parts.iter().filter_map(|part| if let PartKind::Picture { path, .. } = &part.kind { Some(path.clone()) } else { None }).collect()
    }

    #[test]
    fn answers_show_pictures_they_link_or_mention_once_each_right_after_the_text() {
        let dir = tempfile::tempdir().expect("dir");
        let path = saved_picture(dir.path(), "fox.png");
        let shown = path.display().to_string().replace('\\', "/");
        let text = format!("Here it is.\n\n[Open the picture]({shown})\n\nSaved at `{shown}`.\n\nThe end, see C:/not/there.png");
        let parts = block_parts(&Block::Assistant { label: None, text });
        assert_eq!(picture_paths(&parts).len(), 1, "same picture once, missing files none");
        assert!(matches!(parts[2].kind, PartKind::Picture { .. }), "it sits right under its link");
        assert!(matches!(parts[2].kind, PartKind::Picture { width, height, .. } if (width - 640.).abs() < 0.1 && (height - 360.).abs() < 0.1));
    }

    #[test]
    fn attached_pictures_show_in_your_message_instead_of_their_path() {
        let dir = tempfile::tempdir().expect("dir");
        let path = saved_picture(dir.path(), "shot.png");
        let parts = block_parts(&Block::User(format!("what is this?\n\nAttached: {}\nAttached: C:/notes.txt", path.display())));
        assert_eq!(parts[0].rich.text, "what is this?\n\nAttached: C:/notes.txt");
        assert_eq!(picture_paths(&parts), [path]);
    }

    #[test]
    fn small_pictures_are_not_blown_up_and_tall_ones_fit() {
        assert_eq!(picture_size(100, 50), (100., 50.));
        assert_eq!(picture_size(840, 1680), (210., 420.));
    }

    #[test]
    fn links_open_as_web_addresses_or_files() {
        let cwd = Path::new("C:/work");
        assert_eq!(link_target("https://a.se/x%20y", cwd), "https://a.se/x%20y");
        assert_eq!(link_target("C:/u/a.png", cwd), "C:/u/a.png");
        assert_eq!(link_target("file:///C:/u/my%20pic.png", cwd), "C:/u/my pic.png");
        assert_eq!(link_target("C:/u/a%20b.png", cwd), "C:/u/a b.png");
        assert!(link_target("src/app.rs:12", cwd).ends_with("src/app.rs"));
        assert!(link_target("src/app.rs", cwd).starts_with("C:/work"));
    }

    #[test]
    fn every_block_kind_produces_text_with_state_in_words() {
        let tasks = Block::Tasks { phase: "Building".into(), rows: vec![TaskRow { id: "a".into(), title: "Add limiter".into(), state: TaskState::Failed, target: Some("claude:opus".into()), note: "checks failed\nmore".into() }] };
        let parts = block_parts(&tasks);
        assert_eq!(parts[0].rich.text, "Building");
        assert_eq!(parts[1].rich.text, "Add limiter  failed  claude:opus  checks failed");
        let tool = Block::Tool { id: "1".into(), name: "Read".into(), detail: "a.rs".into(), status: ToolStatus::Failed, summary: "not found".into() };
        assert_eq!(block_parts(&tool)[0].rich.text, "Read  a.rs  not found");
        let answer = Block::Assistant { label: Some("codex:gpt".into()), text: "# Plan\n\n- **one**\n\n```rust\nfn x() {}\n```".into() };
        let parts = block_parts(&answer);
        assert_eq!(parts.iter().map(|p| p.rich.text.as_str()).collect::<Vec<_>>(), ["codex:gpt", "Plan", "one", "fn x() {}"]);
        assert!(parts[1].rich.spans.iter().all(|span| span.1.bold), "headings are bold");
    }

    #[test]
    fn tables_copy_as_tab_separated_text_with_a_bold_header() {
        let answer = Block::Assistant { label: None, text: "| Area | Note |\n|---|---|\n| Login | **Posts** to .info |".into() };
        let parts = block_parts(&answer);
        assert_eq!(parts[0].rich.text, "Area\tNote\nLogin\tPosts to .info");
        let PartKind::Table { rows, .. } = &parts[0].kind else { panic!("table") };
        assert!(rows[0][0].spans[0].1.bold, "the header stands out");
    }

    #[test]
    fn long_columns_get_more_room_but_short_ones_stay_readable() {
        let cell = |text: &str| Rich::plain(text, Inline::default());
        let rows = vec![vec![cell("-"), cell("a medium length value"), cell(&"long ".repeat(40))]];
        let shares = column_shares(&rows);
        assert!((shares.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert!(shares[0] < shares[1] && shares[1] < shares[2]);
        assert!(shares[0] >= 6.0 / 71.0 - 1e-5, "a tiny column keeps a minimum width");
        assert!(column_shares(&[]).is_empty());
    }

    #[test]
    fn long_tool_details_stay_reachable_when_expanded() {
        let tool = Block::Tool { id: "1".into(), name: "Bash".into(), detail: "line one\nline two".into(), status: ToolStatus::Ok, summary: "done".into() };
        let parts = block_parts(&tool);
        assert_eq!(parts[0].rich.text, "Bash  line one");
        assert!(matches!(&parts[0].kind, PartKind::Tool { detail, .. } if detail == "line one\nline two\n\ndone"));
    }

    #[test]
    fn selection_ranges_work_forward_backward_and_across_parts() {
        let forward = selection(pos(0, 0, 2), pos(0, 0, 5));
        assert_eq!(forward.range_in(1, 0, 0, 10), Some(2..5));
        assert_eq!(selection(pos(0, 0, 5), pos(0, 0, 2)).range_in(1, 0, 0, 10), Some(2..5));
        let across = selection(pos(0, 1, 3), pos(2, 0, 4));
        assert_eq!(across.range_in(1, 0, 0, 10), None);
        assert_eq!(across.range_in(1, 0, 1, 10), Some(3..10));
        assert_eq!(across.range_in(1, 1, 0, 10), Some(0..10));
        assert_eq!(across.range_in(1, 2, 0, 10), Some(0..4));
        assert_eq!(across.range_in(2, 1, 0, 10), None, "other chats are unaffected");
    }

    #[test]
    fn copying_joins_only_the_selected_text() {
        let blocks = vec![Block::User("hello world".into()), Block::Note("second".into()), Block::Note("third".into())];
        assert_eq!(selection(pos(0, 0, 6), pos(2, 0, 3)).text(&blocks), "world\nsecond\nthi");
    }

    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        assert!(!selection(pos(0, 0, 2), pos(0, 0, 2)).has_text());
    }

    #[test]
    fn spans_split_exactly_at_selection_edges_and_keep_their_length() {
        let bold = Inline { bold: true, ..Inline::default() };
        let mut rich = Rich::default();
        rich.push("abc", Inline::default());
        rich.push("def", bold);
        let split = split_for_selection(&rich, Some(&(1..4)));
        assert_eq!(split, [(1, Inline::default(), false), (2, Inline::default(), true), (1, bold, true), (2, bold, false)]);
        assert_eq!(split_for_selection(&rich, None).len(), 2);
    }

    #[test]
    fn double_click_finds_the_whole_word_including_multibyte() {
        assert_eq!(word_at("hello brave_new world", 8), 6..15);
        assert_eq!(word_at("åäö öl", 2), 0.."åäö".len());
        assert_eq!(word_at("a  b", 2), 2..2);
    }
}
