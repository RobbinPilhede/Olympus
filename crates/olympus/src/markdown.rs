use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::ops::Range;

/// A semantic text color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    #[default]
    Text,
    Dim,
    Accent,
    Red,
    Bright,
}

/// How one stretch of text looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Inline {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: bool,
    pub tone: Tone,
}

/// Styled text; span lengths are UTF-8 bytes and add up to the text's length.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rich {
    pub text: String,
    pub spans: Vec<(usize, Inline)>,
    /// Where each link's text sits, and where it goes.
    pub links: Vec<(Range<usize>, String)>,
}

impl Rich {
    /// Text in one style.
    pub fn plain(text: impl Into<String>, style: Inline) -> Self {
        let mut rich = Self::default();
        rich.push(&text.into(), style);
        rich
    }

    /// Appends text, merging with the last span when the style matches.
    pub fn push(&mut self, text: &str, style: Inline) {
        if text.is_empty() {
            return;
        }
        self.text.push_str(text);
        match self.spans.last_mut() {
            Some((len, last)) if *last == style => *len += text.len(),
            _ => self.spans.push((text.len(), style)),
        }
    }
}

/// One block of rendered markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Md {
    Para(Rich),
    Heading { level: u8, text: Rich },
    Item { depth: usize, marker: String, text: Rich },
    Quote(Rich),
    Code { lang: String, text: String },
    Table(Vec<Vec<Rich>>),
    Rule,
}

#[derive(Default)]
struct Builder {
    out: Vec<Md>,
    current: Rich,
    style: Inline,
    stack: Vec<Inline>,
    lists: Vec<Option<u64>>,
    pending_marker: Option<(usize, String)>,
    heading: Option<u8>,
    quote: u32,
    code: Option<(String, String)>,
    table: Option<Vec<Vec<Rich>>>,
    cell: Rich,
    open_links: Vec<(usize, String)>,
}

impl Builder {
    fn flush(&mut self) {
        if self.current.text.is_empty() && self.pending_marker.is_none() {
            return;
        }
        let text = std::mem::take(&mut self.current);
        let block = if let Some((depth, marker)) = self.pending_marker.take() {
            Md::Item { depth, marker, text }
        } else if let Some(level) = self.heading {
            Md::Heading { level, text }
        } else if self.quote > 0 {
            Md::Quote(text)
        } else {
            Md::Para(text)
        };
        self.out.push(block);
    }

    fn enter(&mut self, change: impl FnOnce(&mut Inline)) {
        self.stack.push(self.style);
        change(&mut self.style);
    }

    fn leave(&mut self) {
        self.style = self.stack.pop().unwrap_or_default();
    }

    fn write(&mut self, text: &str) {
        let style = self.style;
        if self.table.is_some() {
            self.cell.push(text, style);
        } else {
            self.current.push(text, style);
        }
    }

    fn rich_mut(&mut self) -> &mut Rich {
        if self.table.is_some() {
            &mut self.cell
        } else {
            &mut self.current
        }
    }

    fn open_link(&mut self, target: &str) {
        let start = self.rich_mut().text.len();
        self.open_links.push((start, target.to_string()));
        self.enter(|s| {
            s.link = true;
            s.tone = Tone::Bright;
        });
    }

    fn close_link(&mut self) {
        if let Some((start, target)) = self.open_links.pop() {
            // A picture with no description still needs something to click.
            if self.rich_mut().text.len() == start {
                let name = target.rsplit(['/', '\\']).next().unwrap_or(&target).to_string();
                self.write(&name);
            }
            let rich = self.rich_mut();
            let end = rich.text.len();
            rich.links.push((start..end, target));
        }
        self.leave();
    }

    fn finish_code(&mut self) {
        if let Some((lang, text)) = self.code.take() {
            self.out.push(Md::Code { lang, text: text.trim_end_matches('\n').to_string() });
        }
    }

    fn finish_table(&mut self) {
        if let Some(rows) = self.table.take() {
            self.out.push(Md::Table(rows));
        }
    }
}

/// Parses markdown; incomplete streaming input is fine.
pub fn parse(text: &str) -> Vec<Md> {
    let mut b = Builder::default();
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    b.flush();
                    b.heading = Some(match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        _ => 3,
                    });
                }
                Tag::BlockQuote(_) => {
                    b.flush();
                    b.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    b.flush();
                    let lang = match kind {
                        CodeBlockKind::Fenced(lang) => lang.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    b.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    b.flush();
                    b.lists.push(start);
                }
                Tag::Item => {
                    b.flush();
                    let depth = b.lists.len().saturating_sub(1);
                    let marker = match b.lists.last_mut() {
                        Some(Some(n)) => {
                            let marker = format!("{n}.");
                            *n += 1;
                            marker
                        }
                        _ => "•".to_string(),
                    };
                    b.pending_marker = Some((depth, marker));
                }
                Tag::Table(_) => {
                    b.flush();
                    b.table = Some(Vec::new());
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(rows) = &mut b.table {
                        rows.push(Vec::new());
                    }
                }
                Tag::Emphasis => b.enter(|s| s.italic = true),
                Tag::Strong => b.enter(|s| s.bold = true),
                Tag::Strikethrough => b.enter(|s| s.strike = true),
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => b.open_link(&dest_url),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    b.flush();
                    b.heading = None;
                }
                TagEnd::Paragraph | TagEnd::Item => b.flush(),
                TagEnd::BlockQuote(_) => {
                    b.flush();
                    b.quote = b.quote.saturating_sub(1);
                }
                TagEnd::CodeBlock => b.finish_code(),
                TagEnd::List(_) => {
                    b.flush();
                    b.lists.pop();
                }
                TagEnd::TableCell => {
                    let cell = std::mem::take(&mut b.cell);
                    if let Some(row) = b.table.as_mut().and_then(|rows| rows.last_mut()) {
                        row.push(cell);
                    }
                }
                TagEnd::Table => b.finish_table(),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => b.leave(),
                TagEnd::Link | TagEnd::Image => b.close_link(),
                _ => {}
            },
            Event::Text(content) => match &mut b.code {
                Some((_, code)) => code.push_str(&content),
                None => b.write(&content),
            },
            Event::Code(content) => {
                let style = Inline { code: true, ..b.style };
                if b.table.is_some() {
                    b.cell.push(&content, style);
                } else {
                    b.current.push(&content, style);
                }
            }
            Event::SoftBreak => b.write(" "),
            Event::HardBreak => b.write("\n"),
            Event::Rule => {
                b.flush();
                b.out.push(Md::Rule);
            }
            Event::TaskListMarker(done) => b.write(if done { "[x] " } else { "[ ] " }),
            Event::Html(content) | Event::InlineHtml(content) => b.write(&content),
            _ => {}
        }
    }
    b.flush();
    b.finish_code();
    b.finish_table();
    b.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(blocks: &[Md]) -> Vec<String> {
        blocks
            .iter()
            .map(|block| match block {
                Md::Para(t) | Md::Quote(t) | Md::Heading { text: t, .. } => t.text.clone(),
                Md::Item { marker, text, .. } => format!("{marker} {}", text.text),
                Md::Code { text, .. } => text.clone(),
                Md::Table(rows) => rows.iter().map(|row| row.iter().map(|cell| cell.text.as_str()).collect::<Vec<_>>().join("|")).collect::<Vec<_>>().join("/"),
                Md::Rule => "---".into(),
            })
            .collect()
    }

    #[test]
    fn renders_structure_without_markup_characters() {
        let blocks = parse("# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n\n1. first\n2. second");
        assert_eq!(texts(&blocks), ["Title", "Some bold and code.", "• one", "• two", "1. first", "2. second"]);
        assert!(matches!(blocks[0], Md::Heading { level: 1, .. }));
    }

    #[test]
    fn inline_styles_become_spans_that_add_up_to_the_text() {
        let blocks = parse("a **b** `c` *d* ~~e~~ [f](http://x)");
        let Md::Para(rich) = &blocks[0] else { panic!("para") };
        assert_eq!(rich.spans.iter().map(|s| s.0).sum::<usize>(), rich.text.len());
        assert!(rich.spans.iter().any(|s| s.1.bold) && rich.spans.iter().any(|s| s.1.code) && rich.spans.iter().any(|s| s.1.italic));
        assert!(rich.spans.iter().any(|s| s.1.strike) && rich.spans.iter().any(|s| s.1.link && s.1.tone == Tone::Bright));
    }

    #[test]
    fn links_and_pictures_remember_where_they_go_even_in_tables() {
        let blocks = parse("see [the file](C:/a/b.png) and ![](C:/a/c.png)\n\n| x |\n|---|\n| [y](http://z) |");
        let Md::Para(rich) = &blocks[0] else { panic!("para") };
        assert_eq!(rich.text, "see the file and c.png");
        assert_eq!(rich.links, [(4..12, "C:/a/b.png".to_string()), (17..22, "C:/a/c.png".to_string())]);
        assert_eq!(rich.spans.iter().map(|s| s.0).sum::<usize>(), rich.text.len());
        let Md::Table(rows) = &blocks[1] else { panic!("table") };
        assert_eq!(rows[1][0].links, [(0..1, "http://z".to_string())]);
    }

    #[test]
    fn code_blocks_keep_their_lines_and_language() {
        assert_eq!(parse("```rust\nfn a() {}\n  b\n```"), [Md::Code { lang: "rust".into(), text: "fn a() {}\n  b".into() }]);
    }

    #[test]
    fn unterminated_streaming_markdown_still_renders() {
        assert_eq!(texts(&parse("text **half")), ["text **half"]);
        assert_eq!(parse("```\ncode"), [Md::Code { lang: String::new(), text: "code".into() }]);
    }

    #[test]
    fn tables_keep_their_rows_cells_and_inline_styles() {
        let blocks = parse("| a | long |\n|---|---|\n| **1** | `2` |\n\nafter");
        assert_eq!(texts(&blocks), ["a|long/1|2", "after"]);
        let Md::Table(rows) = &blocks[0] else { panic!("table") };
        assert!(rows[1][0].spans[0].1.bold);
        assert!(rows[1][1].spans[0].1.code);
    }

    #[test]
    fn nested_lists_keep_their_depth() {
        assert!(parse("- one\n  - two").iter().any(|b| matches!(b, Md::Item { depth: 1, .. })));
    }
}
