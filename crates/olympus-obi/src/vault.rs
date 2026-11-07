//! A folder of plain Markdown notes, readable by Obsidian and by us: indexing, links, search and the graph.
use anyhow::{bail, Context, Result};
use chrono::{Datelike, NaiveDate, NaiveDateTime};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, io};

/// One link in a note. `line` is 1-based, counted from the top of the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    pub target: String,
    pub heading: Option<String>,
    pub block: Option<String>,
    pub alias: Option<String>,
    pub embed: bool,
    pub line: usize,
}

/// An indexed note. `modified` is Unix milliseconds; `body_start` is the byte offset after the frontmatter.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Note {
    pub rel: String,
    pub title: String,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub links: Vec<Link>,
    pub headings: Vec<(u8, String)>,
    pub props: Map<String, Value>,
    pub modified: i64,
    pub body_start: usize,
}

/// A line of another note that refers to a given note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Backlink {
    pub from: String,
    pub line: usize,
    pub context: String,
}

/// A link leaving a note; `resolved` is `None` when its target does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outgoing {
    pub link: Link,
    pub resolved: Option<String>,
}

/// One search result. `line` is 1-based, or 0 when the note matched as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub rel: String,
    pub line: usize,
    pub snippet: String,
}

/// How a callout starts: `+` is open and `-` is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Fold {
    Open,
    Closed,
}

/// The first line of a callout, with its kind reduced to a canonical name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Callout {
    pub kind: String,
    pub fold: Option<Fold>,
    pub title: Option<String>,
}

/// What a graph node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NodeKind {
    Note,
    Tag,
    Unresolved,
    Attachment,
}

/// A graph node. `group` is the first matching entry of `GraphFilter::groups`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GNode {
    pub id: String,
    pub label: String,
    pub kind: NodeKind,
    pub degree: usize,
    pub group: Option<usize>,
}

/// The graph as nodes and undirected, deduplicated edges between node indices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GraphData {
    pub nodes: Vec<GNode>,
    pub edges: Vec<(usize, usize)>,
}

/// What the graph shows. `query` and `groups` use the `Vault::search` language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphFilter {
    pub query: String,
    pub show_tags: bool,
    pub show_attachments: bool,
    pub hide_unresolved: bool,
    pub show_orphans: bool,
    pub groups: Vec<String>,
    pub local: Option<(String, usize)>,
}

impl Default for GraphFilter {
    fn default() -> Self {
        Self { query: String::new(), show_tags: false, show_attachments: false, hide_unresolved: false, show_orphans: true, groups: Vec::new(), local: None }
    }
}

/// A parsed search query.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    All,
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
    Text(Pat),
    In(Scope, Box<Query>),
    Tag(String),
    Prop(String, Option<Box<Query>>),
}

/// The part of a note a search operator looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    File,
    Path,
    Content,
    Line,
    Section,
    Task,
    TaskTodo,
    TaskDone,
}

/// A text or regex pattern; `case` means case-sensitive.
#[derive(Debug, Clone)]
pub struct Pat {
    pub text: String,
    pub case: bool,
    regex: Option<Regex>,
}

impl PartialEq for Pat {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.case == other.case && self.regex.is_some() == other.regex.is_some()
    }
}

impl Pat {
    /// A plain substring pattern.
    pub fn text(text: &str, case: bool) -> Self {
        Self { text: text.to_owned(), case, regex: None }
    }

    /// A regex pattern, or `None` if it does not compile within the size limit.
    pub fn regex(source: &str, case: bool) -> Option<Self> {
        let regex = RegexBuilder::new(source).case_insensitive(!case).size_limit(1 << 20).build().ok()?;
        Some(Self { text: source.to_owned(), case, regex: Some(regex) })
    }

    fn is_match(&self, hay: &str) -> bool {
        match &self.regex {
            Some(regex) => regex.is_match(hay),
            None if self.case => hay.contains(&self.text),
            None => hay.to_lowercase().contains(&self.text.to_lowercase()),
        }
    }
}

const MAX_QUERY_DEPTH: usize = 32;

/// Parses a search query. Never fails: incomplete syntax falls back to literal text, so it works while typing.
pub fn parse_query(input: &str) -> Query {
    let mut parser = QueryParser { s: input, i: 0, depth: 0 };
    let mut arms = vec![parser.or(false)];
    while parser.i < input.len() {
        parser.i += 1;
        arms.push(parser.or(false));
    }
    arms.retain(|q| *q != Query::All);
    match arms.len() {
        0 => Query::All,
        1 => arms.remove(0),
        _ => Query::And(arms),
    }
}

struct QueryParser<'a> {
    s: &'a str,
    i: usize,
    depth: usize,
}

impl QueryParser<'_> {
    fn rest(&self) -> &str {
        &self.s[self.i..]
    }

    fn skip_space(&mut self) {
        self.i = self.s.len() - self.rest().trim_start().len();
    }

    fn at_or(&self) -> bool {
        let rest = self.rest();
        rest.strip_prefix("OR").is_some_and(|after| after.chars().next().is_none_or(|c| c.is_whitespace() || c == '(' || c == ')'))
    }

    fn or(&mut self, case: bool) -> Query {
        let mut arms = vec![self.and(case)];
        while self.at_or() {
            self.i += 2;
            arms.push(self.and(case));
        }
        if arms.len() == 1 { arms.remove(0) } else { Query::Or(arms) }
    }

    fn and(&mut self, case: bool) -> Query {
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.rest().is_empty() || self.rest().starts_with(')') || self.at_or() {
                break;
            }
            items.push(self.unary(case));
        }
        match items.len() {
            0 => Query::All,
            1 => items.remove(0),
            _ => Query::And(items),
        }
    }

    fn unary(&mut self, case: bool) -> Query {
        let negated = self.rest().strip_prefix('-').is_some_and(|after| after.chars().next().is_some_and(|c| !c.is_whitespace()));
        if !negated {
            return self.atom(case);
        }
        self.i += 1;
        Query::Not(Box::new(self.atom(case)))
    }

    fn atom(&mut self, case: bool) -> Query {
        if self.rest().starts_with('(') {
            return self.group(case);
        }
        if self.rest().starts_with('[') {
            if let Some(prop) = self.prop(case) {
                return prop;
            }
        }
        const SCOPES: [(&str, Scope); 8] = [("file:", Scope::File), ("path:", Scope::Path), ("content:", Scope::Content), ("line:", Scope::Line), ("section:", Scope::Section), ("task:", Scope::Task), ("task-todo:", Scope::TaskTodo), ("task-done:", Scope::TaskDone)];
        for (name, scope) in SCOPES {
            if self.rest().starts_with(name) {
                self.i += name.len();
                return Query::In(scope, Box::new(self.operand(case)));
            }
        }
        if self.rest().starts_with("tag:") {
            self.i += 4;
            return into_tags(self.operand(case));
        }
        if self.rest().starts_with("match-case:") {
            self.i += 11;
            return self.operand(true);
        }
        if self.rest().starts_with("ignore-case:") {
            self.i += 12;
            return self.operand(false);
        }
        self.text(case)
    }

    fn group(&mut self, case: bool) -> Query {
        if self.depth >= MAX_QUERY_DEPTH {
            self.i = self.s.len();
            return Query::All;
        }
        self.i += 1;
        self.depth += 1;
        let inner = self.or(case);
        self.depth -= 1;
        self.skip_space();
        if self.rest().starts_with(')') {
            self.i += 1;
        }
        inner
    }

    fn operand(&mut self, case: bool) -> Query {
        if self.rest().starts_with('(') {
            return self.group(case);
        }
        if self.rest().chars().next().is_none_or(|c| c.is_whitespace() || c == ')') {
            return Query::All;
        }
        self.text(case)
    }

    fn prop(&mut self, case: bool) -> Option<Query> {
        let end = self.rest().find(']')?;
        let inner = &self.rest()[1..end];
        let (name, value) = match inner.split_once(':') {
            Some((name, value)) => (name.trim(), value.trim()),
            None => (inner.trim(), ""),
        };
        if name.is_empty() {
            return None;
        }
        let name = name.to_owned();
        let value = (!value.is_empty()).then(|| {
            let mut sub = QueryParser { s: value, i: 0, depth: self.depth + 1 };
            if sub.depth < MAX_QUERY_DEPTH { Box::new(sub.or(case)) } else { Box::new(Query::All) }
        });
        self.i += end + 1;
        Some(Query::Prop(name, value))
    }

    fn text(&mut self, case: bool) -> Query {
        let rest = &self.s[self.i..];
        if let Some(body) = rest.strip_prefix('"') {
            let end = closing(body, '"');
            let phrase = body[..end.unwrap_or(body.len())].replace("\\\"", "\"");
            self.i += 1 + end.map_or(body.len(), |e| e + 1);
            return Query::Text(Pat::text(&phrase, case));
        }
        if let Some(body) = rest.strip_prefix('/') {
            if let Some(end) = closing(body, '/').filter(|&e| body[e + 1..].chars().next().is_none_or(|c| c.is_whitespace() || c == ')')) {
                let pat = Pat::regex(&body[..end], case).unwrap_or_else(|| Pat::text(&rest[..end + 2], case));
                self.i += end + 2;
                return Query::Text(pat);
            }
        }
        let len = rest.find(|c: char| c.is_whitespace() || c == ')').unwrap_or(rest.len()).max(rest.chars().next().map_or(0, char::len_utf8));
        let word = &rest[..len];
        self.i += len;
        Query::Text(Pat::text(word, case))
    }
}

fn closing(body: &str, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            return Some(i);
        }
    }
    None
}

fn into_tags(q: Query) -> Query {
    match q {
        Query::And(items) => Query::And(items.into_iter().map(into_tags).collect()),
        Query::Or(items) => Query::Or(items.into_iter().map(into_tags).collect()),
        Query::Not(inner) => Query::Not(Box::new(into_tags(*inner))),
        Query::Text(pat) => Query::Tag(pat.text.trim_start_matches('#').to_lowercase()),
        other => other,
    }
}

/// Splits a note into its YAML frontmatter, if any, and its body.
pub fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let start = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some((first, after)) = start.split_once('\n') else { return (None, text) };
    if first.trim_end() != "---" {
        return (None, text);
    }
    let mut pos = 0;
    for line in after.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (Some(&after[..pos]), &after[pos + line.len()..]);
        }
        pos += line.len();
    }
    (None, text)
}

/// Parses a callout's first line, like `> [!tip]- Title`. Unknown kinds become `note`.
pub fn parse_callout(line: &str) -> Option<Callout> {
    let rest = line.trim_start().strip_prefix('>')?.trim_start_matches([' ', '>', '\t']);
    let (kind, after) = rest.strip_prefix("[!")?.split_once(']')?;
    if kind.is_empty() {
        return None;
    }
    let (fold, title) = match after.chars().next() {
        Some('+') => (Some(Fold::Open), &after[1..]),
        Some('-') => (Some(Fold::Closed), &after[1..]),
        _ => (None, after),
    };
    let title = title.trim();
    let kind = match kind.trim().to_lowercase().as_str() {
        "abstract" | "summary" | "tldr" => "abstract",
        "info" => "info",
        "todo" => "todo",
        "tip" | "hint" | "important" => "tip",
        "success" | "check" | "done" => "success",
        "question" | "help" | "faq" => "question",
        "warning" | "caution" | "attention" => "warning",
        "failure" | "fail" | "missing" => "failure",
        "danger" | "error" => "danger",
        "bug" => "bug",
        "example" => "example",
        "quote" | "cite" => "quote",
        _ => "note",
    };
    Some(Callout { kind: kind.to_owned(), fold, title: (!title.is_empty()).then(|| title.to_owned()) })
}

/// The vault path of a date's daily note.
pub fn daily_rel(date: NaiveDate) -> String {
    format!("Daily/{}.md", date.format("%Y-%m-%d"))
}

/// Fills `{{title}}`, `{{date}}`, `{{time}}` and `{{date:FORMAT}}` with Moment-style formats. Others stay as written.
pub fn apply_template(text: &str, title: &str, now: NaiveDateTime) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let expanded = after.find("}}").and_then(|end| Some((template_value(after[..end].trim(), title, now)?, end)));
        match expanded {
            Some((value, end)) => {
                out.push_str(&value);
                rest = &after[end + 2..];
            }
            None => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn template_value(inner: &str, title: &str, now: NaiveDateTime) -> Option<String> {
    let (name, format) = inner.split_once(':').unwrap_or((inner, ""));
    let format = format.trim();
    match name.trim().to_lowercase().as_str() {
        "title" if format.is_empty() => Some(title.to_owned()),
        "date" => Some(format_moment(if format.is_empty() { "YYYY-MM-DD" } else { format }, now)),
        "time" => Some(format_moment(if format.is_empty() { "HH:mm" } else { format }, now)),
        _ => None,
    }
}

/// Formats a time with Moment.js tokens; `[text]` is literal, and unknown characters pass through.
pub fn format_moment(format: &str, now: NaiveDateTime) -> String {
    const TOKENS: [(&str, &str); 25] = [("YYYY", "%Y"), ("GGGG", "%G"), ("MMMM", "%B"), ("DDDD", "%j"), ("dddd", "%A"), ("MMM", "%b"), ("DDD", "%-j"), ("ddd", "%a"), ("YY", "%y"), ("MM", "%m"), ("DD", "%d"), ("HH", "%H"), ("hh", "%I"), ("mm", "%M"), ("ss", "%S"), ("WW", "%V"), ("M", "%-m"), ("D", "%-d"), ("H", "%-H"), ("h", "%-I"), ("m", "%-M"), ("s", "%-S"), ("W", "%-V"), ("A", "%p"), ("a", "%P")];
    let mut out = String::new();
    let mut rest = format;
    'outer: while let Some(c) = rest.chars().next() {
        if c == '[' {
            if let Some(end) = rest.find(']') {
                out.push_str(&rest[1..end]);
                rest = &rest[end + 1..];
                continue;
            }
        }
        if let Some(after) = rest.strip_prefix("Do") {
            let day = now.day();
            let suffix = match (day % 10, day) {
                (_, 11..=13) => "th",
                (1, _) => "st",
                (2, _) => "nd",
                (3, _) => "rd",
                _ => "th",
            };
            out.push_str(&format!("{day}{suffix}"));
            rest = after;
            continue;
        }
        for (token, spec) in TOKENS {
            if let Some(after) = rest.strip_prefix(token) {
                out.push_str(&now.format(spec).to_string());
                rest = after;
                continue 'outer;
            }
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Normalises a vault path from outside and refuses anything that could leave the vault or reach hidden folders.
pub fn clean_rel(rel: &str) -> Result<String> {
    let rel = rel.replace('\\', "/");
    if rel.is_empty() {
        bail!("empty path");
    }
    if rel.starts_with('/') {
        bail!("absolute paths are not allowed");
    }
    if rel.chars().any(|c| c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*')) {
        bail!("path has characters that are not allowed");
    }
    for part in rel.split('/') {
        if part.is_empty() || part.starts_with('.') {
            bail!("path has an empty, relative or hidden part");
        }
        // Windows silently strips these, which would turn "a.. " into another name.
        if part.ends_with('.') || part.ends_with(' ') {
            bail!("path part ends in a dot or space");
        }
        let stem = part.split('.').next().unwrap_or(part).to_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c: char| c.is_ascii_digit()));
        if device {
            bail!("path names a Windows device");
        }
    }
    Ok(rel)
}

fn is_md(rel: &str) -> bool {
    rel.len() >= 3 && rel.is_char_boundary(rel.len() - 3) && rel[rel.len() - 3..].eq_ignore_ascii_case(".md")
}

fn basename(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, name)| name)
}

fn parent(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn strip_md(rel: &str) -> &str {
    if is_md(rel) { &rel[..rel.len() - 3] } else { rel }
}

fn extension(rel: &str) -> String {
    basename(rel).rsplit_once('.').map_or(String::new(), |(_, ext)| ext.to_lowercase())
}

fn on_disk(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |path, part| path.join(part))
}

struct RawLink {
    link: Link,
    span: Range<usize>,
    whole: Range<usize>,
    markdown: bool,
}

struct Analysis {
    links: Vec<RawLink>,
    masked: String,
    plain: String,
    body_start: usize,
    first_line: usize,
}

fn blank(s: &mut String, range: Range<usize>) {
    let spaces: String = s.as_bytes()[range.clone()].iter().map(|&b| if b == b'\n' { '\n' } else { ' ' }).collect();
    s.replace_range(range, &spaces);
}

/// Replaces fenced and inline code with spaces, keeping every byte offset and line number.
fn mask_code(text: &str) -> String {
    let mut out = text.to_owned();
    let mut fence: Option<(u8, usize, bool)> = None;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let bare = line.trim_start_matches([' ', '\t', '>']);
        let quoted = line[..line.len() - bare.len()].contains('>');
        let mark = bare.as_bytes().first().copied().filter(|b| matches!(b, b'`' | b'~'));
        let run = mark.map_or(0, |m| bare.bytes().take_while(|&b| b == m).count());
        match fence {
            Some((fmark, flen, fquoted)) if !fquoted || quoted => {
                blank(&mut out, pos..pos + line.len());
                if mark == Some(fmark) && run >= flen && bare[run..].trim().is_empty() {
                    fence = None;
                }
                pos += line.len();
                continue;
            }
            _ => fence = None,
        }
        if let Some(m) = mark.filter(|_| run >= 3) {
            if m == b'~' || !bare[run..].contains('`') {
                fence = Some((m, run, quoted));
                blank(&mut out, pos..pos + line.len());
            }
        }
        pos += line.len();
    }
    let spans = inline_code_spans(out.as_bytes());
    for span in spans {
        blank(&mut out, span);
    }
    out
}

fn inline_code_spans(b: &[u8]) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'`' => {
                let run = b[i..].iter().take_while(|&&c| c == b'`').count();
                match closing_run(b, i + run, run) {
                    Some(end) => {
                        spans.push(i..end);
                        i = end;
                    }
                    None => i += run,
                }
            }
            _ => i += 1,
        }
    }
    spans
}

fn closing_run(b: &[u8], from: usize, want: usize) -> Option<usize> {
    let mut j = from;
    while j < b.len() {
        match b[j] {
            b'`' => {
                let run = b[j..].iter().take_while(|&&c| c == b'`').count();
                if run == want {
                    return Some(j + run);
                }
                j += run;
            }
            b'\n' => {
                let next = j + 1 + b[j + 1..].iter().take_while(|c| matches!(c, b' ' | b'\t' | b'\r')).count();
                if next >= b.len() || b[next] == b'\n' {
                    return None;
                }
                j += 1;
            }
            _ => j += 1,
        }
    }
    None
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0).chain(text.bytes().enumerate().filter(|&(_, b)| b == b'\n').map(|(i, _)| i + 1)).collect()
}

fn fragment(frag: &str) -> (Option<String>, Option<String>) {
    let mut parts: Vec<&str> = frag.split('#').map(str::trim).filter(|p| !p.is_empty()).collect();
    let block = match parts.last().and_then(|p| p.strip_prefix('^')) {
        Some(id) => {
            let id = id.to_owned();
            parts.pop();
            Some(id)
        }
        None => None,
    };
    ((!parts.is_empty()).then(|| parts.join("#")), block)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%').then(|| s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())).flatten();
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, ' ' | '%' | '#' | '?' | '(' | ')' | '<' | '>' | '[' | ']' | '^') {
            out.push_str(&format!("%{:02X}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn has_scheme(dest: &str) -> bool {
    let Some((scheme, _)) = dest.split_once(':') else { return false };
    scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

struct MdParts {
    close: usize,
    dest: Range<usize>,
    end: usize,
}

const LINK_REACH: usize = 2000;

fn markdown_parts(b: &[u8], open: usize) -> Option<MdParts> {
    let limit = (open + LINK_REACH).min(b.len());
    let (mut j, mut depth) = (open, 0usize);
    let close = loop {
        if j >= limit {
            return None;
        }
        match b[j] {
            b'\\' => j += 1,
            b'\n' => return None,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            _ => {}
        }
        j += 1;
    };
    if b.get(close + 1) != Some(&b'(') {
        return None;
    }
    let limit = (close + LINK_REACH).min(b.len());
    let mut k = close + 2;
    while k < limit && b[k] == b' ' {
        k += 1;
    }
    if k >= limit {
        return None;
    }
    let dest = if b[k] == b'<' {
        let start = k + 1;
        let len = b[start..limit].iter().position(|&c| c == b'>' || c == b'\n')?;
        if b[start + len] != b'>' {
            return None;
        }
        k = start + len + 1;
        start..start + len
    } else {
        let start = k;
        let mut parens = 0usize;
        while k < limit {
            match b[k] {
                b' ' | b'\t' | b'\n' | b'\r' => break,
                b'\\' => k += 1,
                b'(' => parens += 1,
                b')' if parens == 0 => break,
                b')' => parens -= 1,
                _ => {}
            }
            k += 1;
        }
        start..k.min(limit)
    };
    while k < limit && b[k] == b' ' {
        k += 1;
    }
    if k >= limit {
        return None;
    }
    if let Some(&quote) = b.get(k).filter(|c| matches!(c, b'"' | b'\'')) {
        let len = b[k + 1..limit].iter().position(|&c| c == quote)?;
        k += len + 2;
        while k < limit && b[k] == b' ' {
            k += 1;
        }
    }
    (b.get(k) == Some(&b')')).then_some(MdParts { close, dest, end: k + 1 })
}

fn wiki_link(inner: &str, start: usize, whole: Range<usize>, embed: bool, line: usize) -> Option<RawLink> {
    let (head, alias) = match inner.split_once('|') {
        // Inside a table the pipe is escaped as `\|`.
        Some((head, alias)) => (head.strip_suffix('\\').unwrap_or(head), Some(alias.trim())),
        None => (inner, None),
    };
    let (raw_target, frag) = head.split_once('#').unwrap_or((head, ""));
    let target = raw_target.trim();
    let (heading, block) = fragment(frag);
    if target.is_empty() && heading.is_none() && block.is_none() {
        return None;
    }
    let lead = raw_target.len() - raw_target.trim_start().len();
    let link = Link { target: target.to_owned(), heading, block, alias: alias.filter(|a| !a.is_empty()).map(str::to_owned), embed, line };
    Some(RawLink { link, span: start + lead..start + lead + target.len(), whole, markdown: false })
}

fn markdown_link(src: &str, parts: &MdParts, open: usize, base: usize, embed: bool, line: usize) -> Option<RawLink> {
    let dest = &src[parts.dest.clone()];
    if dest.is_empty() || dest.starts_with("//") || has_scheme(dest) {
        return None;
    }
    let (path, frag) = dest.split_once('#').unwrap_or((dest, ""));
    let target = percent_decode(path);
    let (heading, block) = fragment(&percent_decode(frag));
    if target.is_empty() && heading.is_none() && block.is_none() {
        return None;
    }
    let alias = src[open + 1..parts.close].trim();
    let link = Link { target, heading, block, alias: (!alias.is_empty()).then(|| alias.to_owned()), embed, line };
    let from = open - usize::from(embed);
    Some(RawLink { link, span: base + parts.dest.start..base + parts.dest.start + path.len(), whole: base + from..base + parts.end, markdown: true })
}

fn scan_links(src: &str, masked: &str, base: usize, starts: &[usize], out: &mut Vec<RawLink>) {
    let b = masked.as_bytes();
    let line_at = |offset: usize| starts.partition_point(|&s| s <= base + offset);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                i += 2;
                continue;
            }
            b'[' => {}
            _ => {
                i += 1;
                continue;
            }
        }
        let embed = i > 0 && b[i - 1] == b'!';
        if b.get(i + 1) == Some(&b'[') {
            let limit = (i + 2 + LINK_REACH).min(b.len());
            let end = b[i + 2..limit].windows(2).position(|w| w == b"]]").map(|p| i + 2 + p);
            if let Some(end) = end.filter(|&e| !b[i + 2..e].iter().any(|c| matches!(c, b'\n' | b'['))) {
                let from = i - usize::from(embed);
                out.extend(wiki_link(&src[i + 2..end], base + i + 2, base + from..base + end + 2, embed, line_at(i)));
                i = end + 2;
                continue;
            }
        }
        if let Some(parts) = markdown_parts(b, i) {
            out.extend(markdown_link(src, &parts, i, base, embed, line_at(i)));
        }
        i += 1;
    }
}

fn quoted_ranges(front: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut pos = 0;
    for line in front.split_inclusive('\n') {
        let b = line.as_bytes();
        let (mut i, mut prev) = (0, None);
        while i < b.len() {
            let c = b[i];
            if c == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
                break;
            }
            // An apostrophe inside a plain value does not open a string.
            if matches!(c, b'"' | b'\'') && matches!(prev, None | Some(b':' | b'-' | b'[' | b',')) {
                let mut j = i + 1;
                while j < b.len() && b[j] != c {
                    j += if c == b'"' && b[j] == b'\\' { 2 } else { 1 };
                }
                if j >= b.len() {
                    break;
                }
                out.push(pos + i + 1..pos + j);
                i = j + 1;
                prev = Some(c);
                continue;
            }
            if !c.is_ascii_whitespace() {
                prev = Some(c);
            }
            i += 1;
        }
        pos += line.len();
    }
    out
}

fn analyse(text: &str) -> Analysis {
    let (front, body) = split_frontmatter(text);
    let body_start = text.len() - body.len();
    let starts = line_starts(text);
    let mut links = Vec::new();
    if let Some(front) = front {
        let base = front.as_ptr() as usize - text.as_ptr() as usize;
        for range in quoted_ranges(front) {
            let segment = &front[range.clone()];
            scan_links(segment, segment, base + range.start, &starts, &mut links);
        }
    }
    let front_links = links.len();
    let masked = mask_code(body);
    scan_links(body, &masked, body_start, &starts, &mut links);
    let mut plain = masked.clone();
    for raw in &links[front_links..] {
        blank(&mut plain, raw.whole.start - body_start..raw.whole.end - body_start);
    }
    Analysis { links, masked, plain, body_start, first_line: text[..body_start].matches('\n').count() }
}

fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/') || (c > '\u{bf}' && !c.is_whitespace() && !c.is_control() && !matches!(c, '\u{2000}'..='\u{200b}' | '\u{200e}'..='\u{206f}' | '\u{3000}'..='\u{303f}'))
}

fn valid_tag(tag: &str) -> bool {
    !tag.starts_with('/') && tag.chars().all(is_tag_char) && tag.chars().any(|c| !c.is_numeric() && !matches!(c, '-' | '_' | '/'))
}

fn add_tag(tags: &mut Vec<String>, seen: &mut HashSet<String>, tag: &str) {
    let tag = tag.trim_end_matches('/');
    if valid_tag(tag) && seen.insert(tag.to_lowercase()) {
        tags.push(tag.to_owned());
    }
}

fn body_tags(plain: &str, tags: &mut Vec<String>, seen: &mut HashSet<String>) {
    for (i, _) in plain.match_indices('#') {
        let before = plain[..i].chars().next_back();
        if before.is_none_or(|c| c.is_whitespace() || matches!(c, '(' | ',')) {
            let rest = &plain[i + 1..];
            let len = rest.find(|c: char| !is_tag_char(c)).unwrap_or(rest.len());
            add_tag(tags, seen, &rest[..len]);
        }
    }
}

fn string_list(value: Option<&Value>, split: impl Fn(char) -> bool) -> Vec<String> {
    match value {
        Some(Value::String(s)) => s.split(split).map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect(),
        Some(Value::Array(items)) => items.iter().filter_map(|item| match item {
            Value::String(s) => Some(s.trim().to_owned()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }).filter(|s| !s.is_empty()).collect(),
        Some(Value::Number(n)) => vec![n.to_string()],
        _ => Vec::new(),
    }
}

fn parse_props(front: &str) -> Map<String, Value> {
    if front.trim().is_empty() {
        return Map::new();
    }
    match serde_saphyr::from_str::<Value>(front) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

fn headings_of(body: &str, masked: &str, first_line: usize) -> Vec<(usize, u8, String)> {
    let mut out: Vec<(usize, u8, String)> = Vec::new();
    let (mut pos, mut prev_text): (usize, Option<(usize, &str)>) = (0, None);
    for (index, line) in masked.split_inclusive('\n').enumerate() {
        let lineno = first_line + index;
        let source = &body[pos..pos + line.len()];
        pos += line.len();
        let bare = line.trim_end();
        let indent = bare.len() - bare.trim_start_matches(' ').len();
        let trimmed = &bare[indent..];
        let level = trimmed.bytes().take_while(|&b| b == b'#').count();
        if indent <= 3 && (1..=6).contains(&level) && trimmed[level..].chars().next().is_none_or(char::is_whitespace) {
            let text = source.trim_end()[indent + level..].trim();
            let closed = text.trim_end_matches('#');
            let text = if closed.is_empty() || closed.ends_with([' ', '\t']) { closed.trim_end() } else { text };
            out.push((lineno, level as u8, text.to_owned()));
            prev_text = None;
            continue;
        }
        let underline = indent <= 3 && !trimmed.is_empty() && (trimmed.bytes().all(|b| b == b'=') || trimmed.bytes().all(|b| b == b'-'));
        if let Some((prev_line, text)) = prev_text.filter(|_| underline) {
            out.push((prev_line, if trimmed.starts_with('=') { 1 } else { 2 }, text.trim().to_owned()));
            prev_text = None;
            continue;
        }
        prev_text = (!trimmed.is_empty()).then_some((lineno, source));
    }
    out
}

fn task_state(line: &str) -> Option<bool> {
    let t = line.trim_start_matches([' ', '\t', '>']);
    let rest = match t.strip_prefix(['-', '*', '+']) {
        Some(rest) => rest,
        None => {
            let digits = t.trim_start_matches(|c: char| c.is_ascii_digit());
            if digits.len() == t.len() {
                return None;
            }
            digits.strip_prefix(['.', ')'])?
        }
    };
    let mut chars = rest.strip_prefix(' ')?.trim_start().strip_prefix('[')?.chars();
    let state = chars.next()?;
    (chars.next() == Some(']')).then_some(state != ' ')
}

#[derive(Debug, Clone)]
struct Cached {
    text: String,
    heading_lines: Vec<usize>,
    tasks: Vec<(usize, bool)>,
}

fn parse_note(rel: &str, text: String, modified: i64) -> (Note, Cached) {
    let analysis = analyse(&text);
    let (front, body) = split_frontmatter(&text);
    let props = front.map(parse_props).unwrap_or_default();
    let prop = |name: &str, other: &str| props.get(name).or_else(|| props.get(other));
    let aliases = string_list(prop("aliases", "alias"), |c| c == ',');
    let (mut tags, mut seen) = (Vec::new(), HashSet::new());
    for tag in string_list(prop("tags", "tag"), |c| c == ',' || c.is_whitespace()) {
        add_tag(&mut tags, &mut seen, tag.trim_start_matches('#'));
    }
    body_tags(&analysis.plain, &mut tags, &mut seen);
    let found = headings_of(body, &analysis.masked, analysis.first_line);
    let tasks = analysis.masked.split('\n').enumerate().filter_map(|(i, line)| Some((analysis.first_line + i, task_state(line)?))).collect();
    let note = Note { rel: rel.to_owned(), title: strip_md(basename(rel)).to_owned(), aliases, tags, links: analysis.links.into_iter().map(|raw| raw.link).collect(), headings: found.iter().map(|(_, level, text)| (*level, text.clone())).collect(), props, modified, body_start: analysis.body_start };
    let cached = Cached { heading_lines: found.iter().map(|h| h.0).collect(), tasks, text: String::new() };
    (note, Cached { text, ..cached })
}

#[derive(Debug, Clone, Default)]
struct Index {
    exact: HashSet<String>,
    names: HashMap<String, Vec<String>>,
    aliases: HashMap<String, Vec<String>>,
}

impl Index {
    fn add(&mut self, rel: String, aliases: &[String]) {
        for alias in aliases {
            self.aliases.entry(alias.to_lowercase()).or_default().push(rel.clone());
        }
        self.names.entry(basename(&rel).to_lowercase()).or_default().push(rel.clone());
        self.exact.insert(rel);
    }

    fn find(&self, form: &str, whole_only: bool) -> Vec<&String> {
        let lower = form.to_lowercase();
        let suffix = format!("/{lower}");
        self.names.get(basename(&lower)).into_iter().flatten().filter(|c| {
            let candidate = c.to_lowercase();
            candidate == lower || (!whole_only && candidate.ends_with(&suffix))
        }).collect()
    }

    fn resolve(&self, target: &str, from: &str) -> Option<String> {
        let target = target.trim().replace('\\', "/");
        if target.is_empty() {
            return self.exact.contains(from).then(|| from.to_owned());
        }
        let relative = target.starts_with("./") || target.starts_with("../");
        let path = if relative { join_relative(from, &target)? } else { target.trim_start_matches('/').to_owned() };
        let forms = if is_md(&path) { vec![path.clone()] } else { vec![path.clone(), format!("{path}.md")] };
        if let Some(form) = forms.iter().find(|f| self.exact.contains(f.as_str())) {
            return Some(form.clone());
        }
        let found: Vec<&String> = forms.iter().flat_map(|form| self.find(form, relative)).collect();
        if let Some(best) = pick(&found, from) {
            return Some(best);
        }
        let by_alias: Vec<&String> = self.aliases.get(&path.to_lowercase()).into_iter().flatten().collect();
        pick(&by_alias, from)
    }
}

fn pick(candidates: &[&String], from: &str) -> Option<String> {
    let key = |c: &&&String| (parent(c) != parent(from), c.matches('/').count(), c.to_string());
    candidates.iter().min_by(|a, b| key(a).cmp(&key(b))).map(|c| (*c).clone())
}

fn join_relative(from: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = parent(from).split('/').filter(|p| !p.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn relative_path(from_dir: &str, to: &str) -> String {
    let from: Vec<&str> = from_dir.split('/').filter(|p| !p.is_empty()).collect();
    let dest: Vec<&str> = to.split('/').collect();
    let common = from.iter().zip(&dest).take_while(|(a, b)| a == b).count();
    let ups = std::iter::repeat_n("..", from.len() - common);
    ups.chain(dest[common..].iter().copied()).collect::<Vec<_>>().join("/")
}

fn new_link_text(world: &Index, raw: &RawLink, want: &str, from: &str) -> String {
    let keep_ext = is_md(&raw.link.target);
    let shown = |path: String| if keep_ext { path } else { strip_md(&path).to_owned() };
    if raw.markdown {
        let path = shown(relative_path(parent(from), want));
        return if world.resolve(&path, from).as_deref() == Some(want) { percent_encode(&path) } else { format!("./{}", percent_encode(&path)) };
    }
    let short = shown(basename(want).to_owned());
    if !raw.link.target.contains('/') && world.resolve(&short, from).as_deref() == Some(want) {
        return short;
    }
    shown(want.to_owned())
}

fn check_new_name(rel: &str) -> Result<()> {
    if rel.contains(['#', '^', '[', ']']) {
        bail!("names with # ^ [ ] cannot be linked to");
    }
    Ok(())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    let mut tries = 0;
    loop {
        match fs::rename(from, to) {
            // Windows refuses while a scanner or another editor briefly holds the target open.
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && tries < 5 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().context("path has no parent folder")?;
    fs::create_dir_all(dir).context("creating the note's folder")?;
    let tmp = dir.join(format!(".{}-{}.tmp", std::process::id(), TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)));
    let written = fs::File::create_new(&tmp).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    }).and_then(|()| replace_file(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.context("writing the note")
}

fn modified_ms(meta: &fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

fn walk(dir: &Path, prefix: &str, notes: &mut Vec<String>, files: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(kind) = entry.file_type() else { continue };
        if name.starts_with('.') || kind.is_symlink() {
            continue;
        }
        let rel = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        if kind.is_dir() {
            let _ = walk(&entry.path(), &rel, notes, files);
        } else if is_md(&rel) {
            notes.push(rel);
        } else {
            files.push(rel);
        }
    }
    Ok(())
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn has_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| !hay[..i].chars().next_back().is_some_and(is_word) && !hay[i + needle.len()..].chars().next().is_some_and(is_word))
}

fn fuzzy_score(query: &[char], name: &str) -> Option<i32> {
    let t: Vec<char> = name.to_lowercase().chars().collect();
    if query.len() > t.len() {
        return None;
    }
    let mut prev: Vec<Option<i32>> = vec![None; t.len()];
    for (qi, qc) in query.iter().enumerate() {
        let mut cur = vec![None; t.len()];
        let mut gap: Option<i32> = None;
        for j in 0..t.len() {
            if t[j] == *qc {
                let start = j == 0 || !t[j - 1].is_alphanumeric();
                let bonus = if j == 0 { 12 } else if start { 8 } else { 1 };
                let reach = if qi == 0 {
                    Some(0)
                } else {
                    let adjacent = if j > 0 { prev[j - 1].map(|s| s + 6) } else { None };
                    adjacent.max(gap.map(|s| s - 1))
                };
                cur[j] = reach.map(|s| s + bonus);
            }
            gap = gap.max(prev[j]);
        }
        prev = cur;
    }
    let exact = if t.as_slice() == query { 100 } else { 0 };
    prev.into_iter().flatten().max().map(|s| s + exact)
}

fn snippet(line: &str) -> String {
    line.trim().chars().take(200).collect()
}

fn prop_strings(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items.iter().flat_map(prop_strings).collect(),
        Value::Number(n) => vec![n.to_string()],
        Value::Bool(b) => vec![b.to_string()],
        _ => Vec::new(),
    }
}

fn matches_str(q: &Query, hay: &str) -> bool {
    match q {
        Query::All => true,
        Query::And(items) => items.iter().all(|i| matches_str(i, hay)),
        Query::Or(items) => items.iter().any(|i| matches_str(i, hay)),
        Query::Not(inner) => !matches_str(inner, hay),
        Query::Text(pat) => pat.is_match(hay),
        _ => false,
    }
}

struct Doc<'a> {
    note: &'a Note,
    cached: &'a Cached,
    lines: Vec<&'a str>,
}

impl<'a> Doc<'a> {
    fn new(note: &'a Note, cached: &'a Cached) -> Self {
        Self { note, cached, lines: cached.text.split('\n').map(|l| l.trim_end_matches('\r')).collect() }
    }

    fn eval(&self, q: &Query, range: Range<usize>, names: bool) -> Option<BTreeSet<usize>> {
        match q {
            Query::All => Some(BTreeSet::new()),
            Query::And(items) => {
                let mut all = BTreeSet::new();
                for item in items {
                    all.extend(self.eval(item, range.clone(), names)?);
                }
                Some(all)
            }
            Query::Or(items) => {
                let hits: Vec<BTreeSet<usize>> = items.iter().filter_map(|item| self.eval(item, range.clone(), names)).collect();
                if hits.is_empty() { None } else { Some(hits.into_iter().flatten().collect()) }
            }
            Query::Not(inner) => if self.eval(inner, range, names).is_some() { None } else { Some(BTreeSet::new()) },
            Query::Text(pat) => {
                let hits: BTreeSet<usize> = range.filter(|&l| pat.is_match(self.lines[l])).collect();
                if !hits.is_empty() || (names && pat.is_match(&self.note.title)) { Some(hits) } else { None }
            }
            Query::In(Scope::File, inner) => matches_str(inner, basename(&self.note.rel)).then(BTreeSet::new),
            Query::In(Scope::Path, inner) => matches_str(inner, &self.note.rel).then(BTreeSet::new),
            Query::In(Scope::Content, inner) => self.eval(inner, range, false),
            Query::In(Scope::Line, inner) => self.any_unit(inner, range.clone().map(|l| l..l + 1)),
            Query::In(Scope::Section, inner) => {
                let heads = self.cached.heading_lines.iter().copied().filter(|l| range.contains(l));
                let bounds: Vec<usize> = std::iter::once(range.start).chain(heads).chain(std::iter::once(range.end)).collect();
                self.any_unit(inner, bounds.windows(2).map(|w| w[0]..w[1]))
            }
            Query::In(scope, inner) => {
                let wanted = |done: bool| match scope {
                    Scope::TaskTodo => !done,
                    Scope::TaskDone => done,
                    _ => true,
                };
                self.any_unit(inner, self.cached.tasks.iter().filter(|(l, done)| range.contains(l) && wanted(*done)).map(|&(l, _)| l..l + 1))
            }
            Query::Tag(want) => {
                let prefix = format!("{want}/");
                let found = self.note.tags.iter().any(|t| { let t = t.to_lowercase(); t == *want || t.starts_with(&prefix) });
                let needle = format!("#{want}");
                found.then(|| range.filter(|&l| self.lines[l].to_lowercase().contains(&needle)).collect())
            }
            Query::Prop(name, value) => {
                let found = self.note.props.iter().find(|(k, _)| k.eq_ignore_ascii_case(name))?.1;
                let ok = value.as_ref().is_none_or(|v| prop_strings(found).iter().any(|s| matches_str(v, s)));
                ok.then(BTreeSet::new)
            }
        }
    }

    fn any_unit(&self, inner: &Query, units: impl Iterator<Item = Range<usize>>) -> Option<BTreeSet<usize>> {
        let mut matched = false;
        let mut all = BTreeSet::new();
        for unit in units {
            let empty = unit.clone();
            if let Some(hits) = self.eval(inner, unit, false) {
                matched = true;
                all.extend(if hits.is_empty() { empty.take(1).collect() } else { hits });
            }
        }
        matched.then_some(all)
    }
}

/// An open vault: its index of notes, tags and links.
#[derive(Debug, Clone)]
pub struct Vault {
    root: PathBuf,
    notes: Vec<Note>,
    cached: Vec<Cached>,
    attachments: Vec<String>,
    index: Index,
}

impl Vault {
    /// Opens a folder and indexes it, skipping dot-folders.
    pub fn open(root: &Path) -> Result<Vault> {
        if !root.is_dir() {
            bail!("the vault folder does not exist");
        }
        let mut vault = Vault { root: root.to_path_buf(), notes: Vec::new(), cached: Vec::new(), attachments: Vec::new(), index: Index::default() };
        vault.rescan()?;
        Ok(vault)
    }

    /// Rebuilds the whole index from disk.
    pub fn rescan(&mut self) -> Result<()> {
        let (mut rels, mut files) = (Vec::new(), Vec::new());
        walk(&self.root, "", &mut rels, &mut files).context("reading the vault folder")?;
        rels.sort();
        files.sort();
        let (mut notes, mut cached) = (Vec::with_capacity(rels.len()), Vec::with_capacity(rels.len()));
        for rel in rels {
            let path = on_disk(&self.root, &rel);
            let Ok(bytes) = fs::read(&path) else { continue };
            let modified = fs::metadata(&path).map_or(0, |m| modified_ms(&m));
            let (note, cache) = parse_note(&rel, String::from_utf8_lossy(&bytes).into_owned(), modified);
            notes.push(note);
            cached.push(cache);
        }
        self.notes = notes;
        self.cached = cached;
        self.attachments = files;
        self.reindex();
        Ok(())
    }

    /// Re-reads one path after it changed on disk, whether added, edited or removed.
    pub fn refresh(&mut self, rel: &str) -> Result<()> {
        let (rel, path) = self.guard(rel)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => return self.rescan(),
            Ok(meta) if meta.is_file() => self.load(&rel, &path, &meta)?,
            _ => {
                let inside = format!("{rel}/");
                if self.notes.iter().any(|n| n.rel.starts_with(&inside)) || self.attachments.iter().any(|a| a.starts_with(&inside)) {
                    return self.rescan();
                }
                self.forget(&rel);
            }
        }
        self.reindex();
        Ok(())
    }

    /// The vault folder as it was opened.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every note, sorted by path.
    pub fn notes(&self) -> &[Note] {
        &self.notes
    }

    /// Every other file in the vault, sorted by path.
    pub fn attachments(&self) -> &[String] {
        &self.attachments
    }

    /// Looks up a note by its exact path.
    pub fn note(&self, rel: &str) -> Option<&Note> {
        self.position(rel).ok().map(|i| &self.notes[i])
    }

    /// Reads a file's current text from disk.
    pub fn read(&self, rel: &str) -> Result<String> {
        let (_, path) = self.guard(rel)?;
        fs::read_to_string(path).context("reading the note")
    }

    /// Replaces or creates a note atomically. Only Markdown files can be written.
    pub fn write(&mut self, rel: &str, text: &str) -> Result<()> {
        let (rel, path) = self.guard(rel)?;
        if !is_md(&rel) {
            bail!("only .md notes can be written");
        }
        atomic_write(&path, text)?;
        self.reload(&rel, &path)
    }

    /// Creates a note, failing if the path is already taken.
    pub fn create(&mut self, rel: &str, text: &str) -> Result<()> {
        let (rel, path) = self.guard(rel)?;
        check_new_name(&rel)?;
        if !is_md(&rel) {
            bail!("only .md notes can be created");
        }
        fs::create_dir_all(path.parent().context("path has no parent folder")?).context("creating the note's folder")?;
        // Claims the name first, so two clients cannot both create it and lose one's text.
        fs::File::create_new(&path).with_context(|| format!("{rel} already exists"))?;
        atomic_write(&path, text)?;
        self.reload(&rel, &path)
    }

    /// Moves a file into the vault's `.trash` folder, keeping its path there.
    pub fn delete(&mut self, rel: &str) -> Result<()> {
        let (rel, path) = self.guard(rel)?;
        if !path.is_file() {
            bail!("{rel} is not a file");
        }
        let trash = self.root.join(".trash");
        self.check_inside(&trash)?;
        let mut dest = on_disk(&trash, &rel);
        let mut copy = 1;
        while dest.symlink_metadata().is_ok() {
            let name = basename(&rel);
            let renamed = match name.rsplit_once('.') {
                Some((stem, ext)) => format!("{stem} {copy}.{ext}"),
                None => format!("{name} {copy}"),
            };
            dest = on_disk(&trash, parent(&rel)).join(renamed);
            copy += 1;
        }
        fs::create_dir_all(dest.parent().context("path has no parent folder")?).context("creating the trash folder")?;
        replace_file(&path, &dest).context("moving to the trash")?;
        self.forget(&rel);
        self.reindex();
        Ok(())
    }

    /// Renames a file and rewrites every link the move would break; returns how many links changed.
    pub fn rename(&mut self, old: &str, new: &str) -> Result<usize> {
        self.rescan()?;
        let (old, old_path) = self.guard(old)?;
        let new = clean_rel(new)?;
        check_new_name(&new)?;
        let new_path = on_disk(&self.root, &new);
        self.check_inside(&new_path)?;
        if old == new {
            return Ok(0);
        }
        if !old_path.is_file() {
            bail!("{old} is not a file");
        }
        if extension(&old) != extension(&new) {
            bail!("a rename must keep the file extension");
        }
        if !old.eq_ignore_ascii_case(&new) && new_path.symlink_metadata().is_ok() {
            bail!("{new} already exists");
        }
        let (edits, count) = self.link_edits(&old, &new)?;
        fs::create_dir_all(new_path.parent().context("path has no parent folder")?).context("creating the target folder")?;
        replace_file(&old_path, &new_path).context("renaming the file")?;
        let written = edits.iter().try_for_each(|(rel, text)| atomic_write(&on_disk(&self.root, rel), text));
        self.rescan()?;
        written.map(|()| count)
    }

    fn link_edits(&self, old: &str, new: &str) -> Result<(Vec<(String, String)>, usize)> {
        let moved = |rel: &str| if rel == old { new.to_owned() } else { rel.to_owned() };
        let mut world = Index::default();
        for note in &self.notes {
            world.add(moved(&note.rel), &note.aliases);
        }
        for file in &self.attachments {
            world.add(moved(file), &[]);
        }
        let (mut edits, mut count) = (Vec::new(), 0);
        for (note, cached) in self.notes.iter().zip(&self.cached) {
            let from = moved(&note.rel);
            let mut changes: Vec<(Range<usize>, String)> = Vec::new();
            for raw in analyse(&cached.text).links {
                let Some(before) = self.index.resolve(&raw.link.target, &note.rel) else { continue };
                let want = moved(&before);
                if world.resolve(&raw.link.target, &from).as_deref() != Some(want.as_str()) {
                    changes.push((raw.span.clone(), new_link_text(&world, &raw, &want, &from)));
                }
            }
            if changes.is_empty() {
                continue;
            }
            // Refusing here protects non-UTF-8 notes, which the index only holds in a lossy form.
            let disk = fs::read_to_string(on_disk(&self.root, &note.rel)).with_context(|| format!("{} cannot be rewritten as text", note.rel))?;
            if disk != cached.text {
                bail!("{} changed during the rename", note.rel);
            }
            let mut text = disk;
            changes.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
            count += changes.len();
            for (span, value) in changes {
                text.replace_range(span, &value);
            }
            edits.push((from, text));
        }
        Ok((edits, count))
    }

    /// Resolves a link target as written in `from_rel`: exact path, then path suffix, then aliases.
    pub fn resolve(&self, link_target: &str, from_rel: &str) -> Option<String> {
        self.index.resolve(link_target, from_rel)
    }

    /// Lines in other notes that link to this one.
    pub fn backlinks(&self, rel: &str) -> Vec<Backlink> {
        let mut out = Vec::new();
        for (note, cached) in self.notes.iter().zip(&self.cached) {
            if note.rel == rel {
                continue;
            }
            for link in &note.links {
                if self.index.resolve(&link.target, &note.rel).as_deref() == Some(rel) {
                    out.push(Backlink { from: note.rel.clone(), line: link.line, context: cached.text.lines().nth(link.line - 1).unwrap_or("").trim().to_owned() });
                }
            }
        }
        out
    }

    /// The links leaving a note, each with where it leads.
    pub fn outgoing(&self, rel: &str) -> Vec<Outgoing> {
        let Some(note) = self.note(rel) else { return Vec::new() };
        note.links.iter().map(|link| Outgoing { link: link.clone(), resolved: self.index.resolve(&link.target, rel) }).collect()
    }

    /// Lines in other notes that name this note's title or an alias without linking to it.
    pub fn unlinked_mentions(&self, rel: &str) -> Vec<Backlink> {
        let Some(target) = self.note(rel) else { return Vec::new() };
        let names: Vec<String> = std::iter::once(&target.title).chain(&target.aliases).map(|n| n.to_lowercase()).filter(|n| !n.is_empty()).collect();
        let mut out = Vec::new();
        for (note, cached) in self.notes.iter().zip(&self.cached) {
            if note.rel == rel {
                continue;
            }
            let lowered = cached.text.to_lowercase();
            if !names.iter().any(|name| lowered.contains(name.as_str())) {
                continue;
            }
            let analysis = analyse(&cached.text);
            let lines: Vec<&str> = cached.text.split('\n').collect();
            for (index, line) in analysis.plain.split('\n').enumerate() {
                let lower = line.to_lowercase();
                if names.iter().any(|name| has_word(&lower, name)) {
                    let at = analysis.first_line + index;
                    out.push(Backlink { from: note.rel.clone(), line: at + 1, context: lines[at].trim().to_owned() });
                }
            }
        }
        out
    }

    /// Every tag with how many notes carry it, sorted by name. Identity ignores case; the first casing seen is shown.
    pub fn tags(&self) -> Vec<(String, usize)> {
        let mut seen: HashMap<String, (String, usize)> = HashMap::new();
        for tag in self.notes.iter().flat_map(|n| &n.tags) {
            seen.entry(tag.to_lowercase()).or_insert_with(|| (tag.clone(), 0)).1 += 1;
        }
        let mut tags: Vec<(String, (String, usize))> = seen.into_iter().collect();
        tags.sort_by(|a, b| a.0.cmp(&b.0));
        tags.into_iter().map(|(_, entry)| entry).collect()
    }

    /// Searches notes with the query language of `parse_query`; an empty query finds nothing.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let query = parse_query(query);
        if query == Query::All {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (note, cached) in self.notes.iter().zip(&self.cached) {
            if out.len() >= limit {
                break;
            }
            let doc = Doc::new(note, cached);
            let Some(lines) = doc.eval(&query, 0..doc.lines.len(), true) else { continue };
            if lines.is_empty() {
                out.push(Hit { rel: note.rel.clone(), line: 0, snippet: note.title.clone() });
            }
            out.extend(lines.into_iter().map(|l| Hit { rel: note.rel.clone(), line: l + 1, snippet: snippet(doc.lines[l]) }));
        }
        out.truncate(limit);
        out
    }

    /// Fuzzy-matches titles and aliases for a quick switcher; an empty query lists the most recently modified.
    pub fn switcher(&self, query: &str, limit: usize) -> Vec<String> {
        let query: Vec<char> = query.trim().to_lowercase().chars().collect();
        let mut scored: Vec<(i32, i64, &Note)> = if query.is_empty() {
            self.notes.iter().map(|n| (0, n.modified, n)).collect()
        } else {
            self.notes.iter().filter_map(|n| {
                let best = std::iter::once(&n.title).chain(&n.aliases).filter_map(|name| fuzzy_score(&query, name)).max()?;
                Some((best, 0, n))
            }).collect()
        };
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.title.len().cmp(&b.2.title.len())).then(a.2.rel.cmp(&b.2.rel)));
        scored.into_iter().take(limit).map(|(_, _, n)| n.rel.clone()).collect()
    }

    /// Builds the link graph under a filter.
    pub fn graph(&self, filter: &GraphFilter) -> GraphData {
        let query = parse_query(&filter.query);
        let groups: Vec<Query> = filter.groups.iter().map(|g| parse_query(g)).collect();
        let local_root = filter.local.as_ref().map(|(rel, _)| rel.as_str());
        let mut nodes: Vec<GNode> = Vec::new();
        let mut ids: HashMap<String, usize> = HashMap::new();
        for (note, cached) in self.notes.iter().zip(&self.cached) {
            let doc = Doc::new(note, cached);
            let whole = 0..doc.lines.len();
            if local_root == Some(note.rel.as_str()) || doc.eval(&query, whole.clone(), true).is_some() {
                let group = groups.iter().position(|g| doc.eval(g, whole.clone(), true).is_some());
                ids.insert(note.rel.clone(), nodes.len());
                nodes.push(GNode { id: note.rel.clone(), label: note.title.clone(), kind: NodeKind::Note, degree: 0, group });
            }
        }
        let note_count = nodes.len();
        let mut extra = |nodes: &mut Vec<GNode>, id: String, label: &str, kind: NodeKind| *ids.entry(id.clone()).or_insert_with(|| {
            nodes.push(GNode { id, label: label.to_owned(), kind, degree: 0, group: None });
            nodes.len() - 1
        });
        if filter.show_attachments {
            for file in &self.attachments {
                extra(&mut nodes, file.clone(), basename(file), NodeKind::Attachment);
            }
        }
        let mut edges: BTreeSet<(usize, usize)> = BTreeSet::new();
        for node in 0..note_count {
            let Ok(position) = self.position(&nodes[node].id.clone()) else { continue };
            let note = &self.notes[position];
            for link in &note.links {
                let other = match self.index.resolve(&link.target, &note.rel) {
                    Some(rel) if self.position(&rel).is_ok() => {
                        let existing = nodes[..note_count].binary_search_by(|n| n.id.as_str().cmp(&rel)).ok();
                        let Some(found) = existing else { continue };
                        found
                    }
                    Some(rel) => {
                        if !filter.show_attachments {
                            continue;
                        }
                        extra(&mut nodes, rel.clone(), basename(&rel), NodeKind::Attachment)
                    }
                    None => {
                        if filter.hide_unresolved {
                            continue;
                        }
                        extra(&mut nodes, format!("unresolved:{}", strip_md(&link.target).to_lowercase()), strip_md(&link.target), NodeKind::Unresolved)
                    }
                };
                if other != node {
                    edges.insert((node.min(other), node.max(other)));
                }
            }
            if filter.show_tags {
                for tag in &note.tags {
                    let other = extra(&mut nodes, format!("tag:{}", tag.to_lowercase()), &format!("#{tag}"), NodeKind::Tag);
                    edges.insert((node.min(other), node.max(other)));
                }
            }
        }
        compact_graph(nodes, edges, filter)
    }

    fn position(&self, rel: &str) -> std::result::Result<usize, usize> {
        self.notes.binary_search_by(|n| n.rel.as_str().cmp(rel))
    }

    fn guard(&self, rel: &str) -> Result<(String, PathBuf)> {
        let rel = clean_rel(rel)?;
        let rel = self.index.find(&rel, true).first().map_or(rel.clone(), |known| (*known).clone());
        let path = on_disk(&self.root, &rel);
        self.check_inside(&path)?;
        Ok((rel, path))
    }

    fn check_inside(&self, path: &Path) -> Result<()> {
        let root = self.root.canonicalize().context("the vault folder is gone")?;
        let existing = path.ancestors().find(|p| p.symlink_metadata().is_ok()).context("path has no existing parent")?;
        let real = existing.canonicalize().context("path cannot be followed")?;
        if !real.starts_with(&root) {
            bail!("path leaves the vault");
        }
        Ok(())
    }

    fn reload(&mut self, rel: &str, path: &Path) -> Result<()> {
        let meta = fs::metadata(path).context("reading the note's details")?;
        self.load(rel, path, &meta)?;
        self.reindex();
        Ok(())
    }

    fn load(&mut self, rel: &str, path: &Path, meta: &fs::Metadata) -> Result<()> {
        if !is_md(rel) {
            if let Err(at) = self.attachments.binary_search_by(|a| a.as_str().cmp(rel)) {
                self.attachments.insert(at, rel.to_owned());
            }
            return Ok(());
        }
        let bytes = fs::read(path).context("reading the note")?;
        let (note, cached) = parse_note(rel, String::from_utf8_lossy(&bytes).into_owned(), modified_ms(meta));
        match self.position(rel) {
            Ok(at) => {
                self.notes[at] = note;
                self.cached[at] = cached;
            }
            Err(at) => {
                self.notes.insert(at, note);
                self.cached.insert(at, cached);
            }
        }
        Ok(())
    }

    fn forget(&mut self, rel: &str) {
        if let Ok(at) = self.position(rel) {
            self.notes.remove(at);
            self.cached.remove(at);
        }
        self.attachments.retain(|a| a != rel);
    }

    fn reindex(&mut self) {
        let mut index = Index::default();
        for note in &self.notes {
            index.add(note.rel.clone(), &note.aliases);
        }
        for file in &self.attachments {
            index.add(file.clone(), &[]);
        }
        self.index = index;
    }
}

fn compact_graph(nodes: Vec<GNode>, edges: BTreeSet<(usize, usize)>, filter: &GraphFilter) -> GraphData {
    let mut keep = vec![true; nodes.len()];
    let root = filter.local.as_ref().and_then(|(rel, _)| nodes.iter().position(|n| n.id == *rel));
    if let Some((_, depth)) = &filter.local {
        keep = vec![false; nodes.len()];
        let mut neighbours = vec![Vec::new(); nodes.len()];
        for &(a, b) in &edges {
            neighbours[a].push(b);
            neighbours[b].push(a);
        }
        let mut queue: VecDeque<(usize, usize)> = root.into_iter().map(|r| (r, 0)).collect();
        if let Some(r) = root {
            keep[r] = true;
        }
        while let Some((at, distance)) = queue.pop_front() {
            if distance < *depth {
                for &next in &neighbours[at] {
                    if !keep[next] {
                        keep[next] = true;
                        queue.push_back((next, distance + 1));
                    }
                }
            }
        }
    }
    let mut degree = vec![0; nodes.len()];
    for &(a, b) in edges.iter().filter(|&&(a, b)| keep[a] && keep[b]) {
        degree[a] += 1;
        degree[b] += 1;
    }
    if !filter.show_orphans {
        for (i, flag) in keep.iter_mut().enumerate() {
            *flag = *flag && (degree[i] > 0 || root == Some(i));
        }
    }
    let mut remap = vec![usize::MAX; nodes.len()];
    let mut out = GraphData::default();
    for (i, mut node) in nodes.into_iter().enumerate() {
        if keep[i] {
            remap[i] = out.nodes.len();
            node.degree = degree[i];
            out.nodes.push(node);
        }
    }
    out.edges = edges.into_iter().filter(|&(a, b)| keep[a] && keep[b]).map(|(a, b)| (remap[a], remap[b])).collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn vault(files: &[(&str, &str)]) -> (TempDir, Vault) {
        let dir = tempfile::tempdir().expect("temp dir");
        for (rel, text) in files {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().expect("parent")).expect("folder");
            fs::write(path, text).expect("write");
        }
        let vault = Vault::open(dir.path()).expect("open");
        (dir, vault)
    }

    fn links(text: &str) -> Vec<Link> {
        analyse(text).links.into_iter().map(|raw| raw.link).collect()
    }

    fn targets(text: &str) -> Vec<String> {
        links(text).into_iter().map(|l| l.target).collect()
    }

    fn tags_of(text: &str) -> Vec<String> {
        parse_note("n.md", text.to_owned(), 0).0.tags
    }

    fn word(text: &str) -> Query {
        Query::Text(Pat::text(text, false))
    }

    fn hit_rels(vault: &Vault, query: &str) -> Vec<String> {
        let mut rels: Vec<String> = vault.search(query, 100).into_iter().map(|h| h.rel).collect();
        rels.dedup();
        rels
    }

    fn time(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).expect("date").and_hms_opt(h, min, 5).expect("time")
    }

    #[test]
    fn plain_wikilink() {
        assert_eq!(links("see [[a]]")[0], Link { target: "a".into(), heading: None, block: None, alias: None, embed: false, line: 1 });
    }

    #[test]
    fn wikilink_keeps_its_extension() {
        assert_eq!(links("[[a.md]]")[0].target, "a.md");
    }

    #[test]
    fn wikilink_alias() {
        assert_eq!(links("[[a|b]]")[0].alias.as_deref(), Some("b"));
    }

    #[test]
    fn same_note_heading() {
        let link = &links("[[#h]]")[0];
        assert_eq!((link.target.as_str(), link.heading.as_deref()), ("", Some("h")));
    }

    #[test]
    fn nested_heading() {
        assert_eq!(links("[[a#h#sub]]")[0].heading.as_deref(), Some("h#sub"));
    }

    #[test]
    fn block_reference() {
        let link = &links("[[a#^id]]")[0];
        assert_eq!((link.block.as_deref(), link.heading.as_deref()), (Some("id"), None));
    }

    #[test]
    fn embed() {
        assert!(links("![[a]]")[0].embed);
    }

    #[test]
    fn table_escaped_pipe_is_an_alias() {
        let link = &links("| [[a\\|b]] |")[0];
        assert_eq!((link.target.as_str(), link.alias.as_deref()), ("a", Some("b")));
    }

    #[test]
    fn markdown_link_is_decoded() {
        let link = &links("[x](a%20b.md#h)")[0];
        assert_eq!((link.target.as_str(), link.alias.as_deref(), link.heading.as_deref()), ("a b.md", Some("x"), Some("h")));
    }

    #[test]
    fn markdown_embed_and_title() {
        let link = &links("![pic](p.png \"Title\")")[0];
        assert_eq!((link.target.as_str(), link.embed), ("p.png", true));
    }

    #[test]
    fn external_links_are_ignored() {
        assert!(links("[x](https://a.b/c.md) [y](mailto:a@b.c) [z](//a.b)").is_empty());
    }

    #[test]
    fn empty_wikilink_is_not_a_link() {
        assert!(links("[[]] [[|x]]").is_empty());
    }

    #[test]
    fn links_in_code_are_ignored() {
        assert!(links("```\n[[a]]\n```\n~~~\n[[b]]\n~~~\n`[[c]]` and ``[[d]] ` x``").is_empty());
    }

    #[test]
    fn unclosed_fence_runs_to_the_end() {
        assert!(links("```\n[[a]]\n\n[[b]]").is_empty());
    }

    #[test]
    fn link_after_code_still_counts_and_keeps_its_line() {
        let found = links("```\ncode\n```\n[[a]] `x`");
        assert_eq!((found[0].target.as_str(), found[0].line), ("a", 4));
    }

    #[test]
    fn unmatched_backtick_hides_nothing() {
        assert_eq!(targets("a ` b [[c]]"), ["c"]);
    }

    #[test]
    fn inline_code_cannot_span_a_blank_line() {
        assert_eq!(targets("a ` b\n\n[[c]] ` d"), ["c"]);
    }

    #[test]
    fn escaped_bracket_is_not_a_link() {
        assert!(links("\\[[a]]").is_empty());
    }

    #[test]
    fn frontmatter_links_must_be_quoted() {
        let text = "---\nup: [[a]]\nsee: \"[[b]]\"\nalso: '[[c]]'\nlist:\n  - \"[[d|e]]\"\n# gone: \"[[f]]\"\nplain: don't [[g]] won't\n---\nbody";
        assert_eq!(targets(text), ["b", "c", "d"]);
    }

    #[test]
    fn frontmatter_link_line_counts_from_the_top() {
        assert_eq!(links("---\nsee: \"[[b]]\"\n---\n[[c]]").iter().map(|l| l.line).collect::<Vec<_>>(), [2, 4]);
    }

    #[test]
    fn frontmatter_splits_with_crlf_and_bom() {
        assert_eq!(split_frontmatter("\u{feff}---\r\na: 1\r\n---\r\nbody"), (Some("a: 1\r\n"), "body"));
    }

    #[test]
    fn empty_frontmatter() {
        assert_eq!(split_frontmatter("---\n---\nb"), (Some(""), "b"));
    }

    #[test]
    fn unclosed_frontmatter_is_body() {
        assert_eq!(split_frontmatter("---\na: 1\n"), (None, "---\na: 1\n"));
    }

    #[test]
    fn horizontal_rule_later_is_not_frontmatter() {
        assert_eq!(split_frontmatter("x\n---\ny\n---\n").0, None);
    }

    #[test]
    fn note_fields_come_from_the_frontmatter() {
        let (note, _) = parse_note("f/My Note.md", "---\naliases: [One, Two]\ntags: [x, \"#y\"]\nstatus: open\ncount: 3\n---\n# Head\n".to_owned(), 9);
        assert_eq!((note.title.as_str(), note.aliases.len(), note.modified), ("My Note", 2, 9));
        assert_eq!(note.tags, ["x", "y"]);
        assert_eq!(note.props["status"], Value::String("open".into()));
        assert_eq!(note.props["count"], Value::from(3));
        assert_eq!(&"---\naliases: [One, Two]\ntags: [x, \"#y\"]\nstatus: open\ncount: 3\n---\n# Head\n"[note.body_start..], "# Head\n");
    }

    #[test]
    fn invalid_yaml_leaves_no_props_and_still_indexes() {
        let (note, _) = parse_note("n.md", "---\na: [\n---\n[[b]]".to_owned(), 0);
        assert!(note.props.is_empty());
        assert_eq!(note.links.len(), 1);
    }

    #[test]
    fn string_aliases_split_on_commas() {
        let (note, _) = parse_note("n.md", "---\nalias: One, Two words\n---\n".to_owned(), 0);
        assert_eq!(note.aliases, ["One", "Two words"]);
    }

    #[test]
    fn tags_in_the_body() {
        assert_eq!(tags_of("#a text #b/c (#d) x#e"), ["a", "b/c", "d"]);
    }

    #[test]
    fn unicode_and_emoji_tags() {
        assert_eq!(tags_of("#日本語 #café #🚀"), ["日本語", "café", "🚀"]);
    }

    #[test]
    fn digits_only_tags_are_rejected() {
        assert_eq!(tags_of("#123 #1a"), ["1a"]);
    }

    #[test]
    fn tags_stop_at_punctuation() {
        assert_eq!(tags_of("#a. #b, #c/"), ["a", "b", "c"]);
    }

    #[test]
    fn headings_are_not_tags() {
        assert!(tags_of("# Title\n## Two").is_empty());
    }

    #[test]
    fn tags_in_code_and_links_are_ignored() {
        assert!(tags_of("`#a`\n```\n#b\n```\n[[x #c]] [y](#d)").is_empty());
    }

    #[test]
    fn frontmatter_tags_as_a_string() {
        assert_eq!(tags_of("---\ntags: one, two three\n---\n"), ["one", "two", "three"]);
    }

    #[test]
    fn numeric_frontmatter_tag_is_rejected() {
        assert_eq!(tags_of("---\ntags: [2024, x]\n---\n"), ["x"]);
    }

    #[test]
    fn tags_count_notes_and_keep_first_casing() {
        let (_d, v) = vault(&[("a.md", "#Work #work"), ("b.md", "#WORK #other")]);
        assert_eq!(v.tags(), [("other".to_owned(), 1), ("Work".to_owned(), 2)]);
    }

    #[test]
    fn headings_of_all_kinds() {
        let (note, _) = parse_note("n.md", "# One\n## Two ##\nThree\n===\nFour\n---\n#### `code` here\n#nope\n```\n# skip\n```\n".to_owned(), 0);
        let found: Vec<(u8, &str)> = note.headings.iter().map(|(l, t)| (*l, t.as_str())).collect();
        assert_eq!(found, [(1, "One"), (2, "Two"), (1, "Three"), (2, "Four"), (4, "`code` here")]);
    }

    #[test]
    fn callout_with_fold_and_title() {
        assert_eq!(parse_callout("> [!TIP]- Be careful"), Some(Callout { kind: "tip".into(), fold: Some(Fold::Closed), title: Some("Be careful".into()) }));
    }

    #[test]
    fn callout_aliases_become_the_canonical_kind() {
        assert_eq!(parse_callout(">[!faq]+").map(|c| (c.kind, c.fold, c.title)), Some(("question".into(), Some(Fold::Open), None)));
    }

    #[test]
    fn unknown_callout_is_a_note() {
        assert_eq!(parse_callout("> [!wibble] Hi").map(|c| c.kind), Some("note".into()));
    }

    #[test]
    fn ordinary_quote_is_not_a_callout() {
        assert_eq!(parse_callout("> just a quote"), None);
        assert_eq!(parse_callout("[!note]"), None);
    }

    #[test]
    fn exact_root_path_wins() {
        let (_d, v) = vault(&[("a.md", ""), ("x/a.md", ""), ("x/from.md", "")]);
        assert_eq!(v.resolve("a.md", "x/from.md").as_deref(), Some("a.md"));
    }

    #[test]
    fn exact_path_without_extension() {
        let (_d, v) = vault(&[("x/y/a.md", ""), ("b/x/y/a.md", "")]);
        assert_eq!(v.resolve("x/y/a", "z.md").as_deref(), Some("x/y/a.md"));
    }

    #[test]
    fn suffix_match_ignores_case() {
        let (_d, v) = vault(&[("Deep/Folder/Note.md", "")]);
        assert_eq!(v.resolve("folder/NOTE", "z.md").as_deref(), Some("Deep/Folder/Note.md"));
    }

    #[test]
    fn suffix_must_end_on_a_folder_edge() {
        let (_d, v) = vault(&[("x/my-note.md", "")]);
        assert_eq!(v.resolve("note", "z.md"), None);
    }

    #[test]
    fn same_folder_beats_shorter_path() {
        let (_d, v) = vault(&[("q/a.md", ""), ("x/y/a.md", ""), ("x/y/from.md", "")]);
        assert_eq!(v.resolve("a", "x/y/from.md").as_deref(), Some("x/y/a.md"));
    }

    #[test]
    fn fewest_components_beats_lexical() {
        let (_d, v) = vault(&[("a/b/n.md", ""), ("z/n.md", "")]);
        assert_eq!(v.resolve("n", "top.md").as_deref(), Some("z/n.md"));
    }

    #[test]
    fn lexical_order_breaks_the_last_tie() {
        let (_d, v) = vault(&[("b/n.md", ""), ("a/n.md", "")]);
        assert_eq!(v.resolve("n", "top.md").as_deref(), Some("a/n.md"));
    }

    #[test]
    fn aliases_only_match_last() {
        let (_d, v) = vault(&[("real.md", ""), ("other.md", "---\naliases: [real, Nick]\n---\n")]);
        assert_eq!(v.resolve("real", "z.md").as_deref(), Some("real.md"));
        assert_eq!(v.resolve("nick", "z.md").as_deref(), Some("other.md"));
    }

    #[test]
    fn relative_targets_resolve_from_the_note() {
        let (_d, v) = vault(&[("x/a.md", ""), ("y/b.md", "")]);
        assert_eq!(v.resolve("../x/a.md", "y/b.md").as_deref(), Some("x/a.md"));
        assert_eq!(v.resolve("../../a.md", "y/b.md"), None);
    }

    #[test]
    fn empty_target_is_the_note_itself() {
        let (_d, v) = vault(&[("a.md", "")]);
        assert_eq!(v.resolve("", "a.md").as_deref(), Some("a.md"));
    }

    #[test]
    fn attachments_resolve_by_full_name() {
        let (_d, v) = vault(&[("img/p.png", "x")]);
        assert_eq!(v.resolve("p.png", "z.md").as_deref(), Some("img/p.png"));
    }

    #[test]
    fn backlinks_carry_line_and_context() {
        let (_d, v) = vault(&[("a.md", "self [[a]]"), ("b.md", "x\n  see [[A|there]] now\n[[missing]]")]);
        assert_eq!(v.backlinks("a.md"), [Backlink { from: "b.md".into(), line: 2, context: "see [[A|there]] now".into() }]);
    }

    #[test]
    fn outgoing_flags_unresolved() {
        let (_d, v) = vault(&[("a.md", "[[b]] [[gone]]"), ("b.md", "")]);
        let out = v.outgoing("a.md");
        assert_eq!(out[0].resolved.as_deref(), Some("b.md"));
        assert_eq!(out[1].resolved, None);
    }

    #[test]
    fn unlinked_mentions_respect_words_links_and_code() {
        let (_d, v) = vault(&[
            ("Rust.md", "---\naliases: [Ferris]\n---\nRust itself"),
            ("b.md", "I like rust.\ntrusty rusted\n[[Rust]] rust-free? no\n`rust`\nmet FERRIS today\n[rust](Rust.md)\n"),
        ]);
        let lines: Vec<usize> = v.unlinked_mentions("Rust.md").iter().map(|m| m.line).collect();
        assert_eq!(lines, [1, 3, 5]);
    }

    #[test]
    fn query_implicit_and_or_precedence() {
        assert_eq!(parse_query("a b OR c"), Query::Or(vec![Query::And(vec![word("a"), word("b")]), word("c")]));
    }

    #[test]
    fn query_parentheses_and_negation() {
        assert_eq!(parse_query("-(a OR b) c"), Query::And(vec![Query::Not(Box::new(Query::Or(vec![word("a"), word("b")]))), word("c")]));
    }

    #[test]
    fn query_phrase() {
        assert_eq!(parse_query("\"a b\""), word("a b"));
    }

    #[test]
    fn query_operators() {
        assert_eq!(parse_query("file:x"), Query::In(Scope::File, Box::new(word("x"))));
        assert_eq!(parse_query("task-todo:(a b)"), Query::In(Scope::TaskTodo, Box::new(Query::And(vec![word("a"), word("b")]))));
    }

    #[test]
    fn query_tags_lose_the_hash_and_case() {
        assert_eq!(parse_query("tag:#Work"), Query::Tag("work".into()));
    }

    #[test]
    fn query_props() {
        assert_eq!(parse_query("[status]"), Query::Prop("status".into(), None));
        assert_eq!(parse_query("[status:open]"), Query::Prop("status".into(), Some(Box::new(word("open")))));
    }

    #[test]
    fn query_case_flags() {
        assert_eq!(parse_query("match-case:Ab"), Query::Text(Pat::text("Ab", true)));
        assert_eq!(parse_query("match-case:(a ignore-case:b)"), Query::And(vec![Query::Text(Pat::text("a", true)), word("b")]));
    }

    #[test]
    fn query_regex_and_broken_regex() {
        assert_eq!(parse_query("/a+/"), Query::Text(Pat::regex("a+", false).expect("regex")));
        assert_eq!(parse_query("/(/"), word("/(/"));
    }

    #[test]
    fn query_path_like_word_is_not_a_regex() {
        assert_eq!(parse_query("/usr/bin"), word("/usr/bin"));
    }

    #[test]
    fn query_tolerates_partial_input() {
        assert_eq!(parse_query(""), Query::All);
        assert_eq!(parse_query("file:"), Query::In(Scope::File, Box::new(Query::All)));
        assert_eq!(parse_query("(a"), word("a"));
        assert_eq!(parse_query("a) b"), Query::And(vec![word("a"), word("b")]));
        assert_eq!(parse_query("[x"), word("[x"));
        assert_eq!(parse_query("-"), word("-"));
        assert_eq!(parse_query("\"a"), word("a"));
        assert_eq!(parse_query("ORANGE"), word("ORANGE"));
    }

    #[test]
    fn query_survives_deep_nesting() {
        let deep = "(".repeat(100_000);
        assert_eq!(parse_query(&deep), Query::All);
    }

    #[test]
    fn query_survives_deep_negation() {
        let deep = "-(".repeat(100_000);
        assert!(matches!(parse_query(&deep), Query::Not(_)));
    }

    #[test]
    fn scanner_survives_hostile_notes() {
        let cases = [format!("[x]({}<", " ".repeat(5000)), format!("[x]({}\"", " ".repeat(5000)), format!("[x]({}", "é".repeat(3000)), format!("[[{}]]", "é".repeat(3000)), "[".repeat(20_000), "`".repeat(5000), "[x](\\".to_owned(), "[[a]".to_owned(), "\\".to_owned(), "---\n\"".to_owned(), "#".repeat(5000)];
        for case in cases {
            let (note, _) = parse_note("n.md", case, 0);
            assert!(note.links.is_empty());
        }
    }

    fn search_vault() -> (TempDir, Vault) {
        vault(&[
            ("work/alpha.md", "---\nstatus: open\nnums: [1, 2]\n---\n# One\nmeeting Notes here #Work/deep\n\n# Two\nbudget\n- [ ] call Bob\n- [x] email Ann\n"),
            ("beta.md", "meeting\nnotes on other line\n```\n- [ ] not a task\n```\n"),
            ("gamma.md", "---\nstatus: closed\n---\nnothing of interest\n"),
        ])
    }

    #[test]
    fn terms_and_across_lines_and_report_each_line() {
        let (_d, v) = search_vault();
        let hits = v.search("meeting notes", 10);
        assert_eq!(hits.iter().map(|h| (h.rel.as_str(), h.line)).collect::<Vec<_>>(), [("beta.md", 1), ("beta.md", 2), ("work/alpha.md", 6)]);
        assert_eq!(hits[2].snippet, "meeting Notes here #Work/deep");
    }

    #[test]
    fn plain_terms_match_file_names() {
        let (_d, v) = search_vault();
        assert_eq!(v.search("gamma", 10), [Hit { rel: "gamma.md".into(), line: 0, snippet: "gamma".into() }]);
    }

    #[test]
    fn content_operator_skips_file_names() {
        let (_d, v) = search_vault();
        assert!(v.search("content:gamma", 10).is_empty());
    }

    #[test]
    fn file_and_path_operators() {
        let (_d, v) = search_vault();
        assert_eq!(hit_rels(&v, "file:alpha"), ["work/alpha.md"]);
        assert!(v.search("file:work", 10).is_empty());
        assert_eq!(hit_rels(&v, "path:work/"), ["work/alpha.md"]);
    }

    #[test]
    fn line_operator_needs_one_line() {
        let (_d, v) = search_vault();
        assert_eq!(hit_rels(&v, "line:(meeting notes)"), ["work/alpha.md"]);
    }

    #[test]
    fn section_operator_needs_one_section() {
        let (_d, v) = search_vault();
        assert!(v.search("section:(meeting budget)", 10).is_empty());
        assert_eq!(hit_rels(&v, "section:(budget call)"), ["work/alpha.md"]);
    }

    #[test]
    fn task_operators() {
        let (_d, v) = search_vault();
        assert_eq!(v.search("task:call", 10).len(), 1);
        assert_eq!(v.search("task-todo:call", 10).len(), 1);
        assert!(v.search("task-done:call", 10).is_empty());
        assert_eq!(v.search("task-done:email", 10)[0].line, 11);
    }

    #[test]
    fn task_operator_with_nothing_lists_every_task() {
        let (_d, v) = search_vault();
        assert_eq!(v.search("task:", 10).len(), 2);
    }

    #[test]
    fn tasks_in_code_do_not_count() {
        let (_d, v) = search_vault();
        assert!(v.search("task:\"not a task\"", 10).is_empty());
    }

    #[test]
    fn tag_operator_matches_nested_tags() {
        let (_d, v) = search_vault();
        assert_eq!(hit_rels(&v, "tag:work"), ["work/alpha.md"]);
        assert!(v.search("tag:wor", 10).is_empty());
    }

    #[test]
    fn prop_operators() {
        let (_d, v) = search_vault();
        assert_eq!(hit_rels(&v, "[status]"), ["gamma.md", "work/alpha.md"]);
        assert_eq!(hit_rels(&v, "[status:open]"), ["work/alpha.md"]);
        assert_eq!(hit_rels(&v, "[nums:2]"), ["work/alpha.md"]);
    }

    #[test]
    fn negation_and_or() {
        let (_d, v) = search_vault();
        assert_eq!(hit_rels(&v, "meeting -budget"), ["beta.md"]);
        assert_eq!(hit_rels(&v, "interest OR budget"), ["gamma.md", "work/alpha.md"]);
    }

    #[test]
    fn case_operators() {
        let (_d, v) = search_vault();
        assert_eq!(hit_rels(&v, "match-case:Notes"), ["work/alpha.md"]);
        assert_eq!(hit_rels(&v, "NOTES"), ["beta.md", "work/alpha.md"]);
    }

    #[test]
    fn regex_search() {
        let (_d, v) = search_vault();
        assert_eq!(hit_rels(&v, "/^bud.et$/"), ["work/alpha.md"]);
    }

    #[test]
    fn search_stops_at_the_limit() {
        let (_d, v) = search_vault();
        assert_eq!(v.search("e", 2).len(), 2);
    }

    #[test]
    fn empty_query_finds_nothing() {
        let (_d, v) = search_vault();
        assert!(v.search("  ", 10).is_empty());
    }

    #[test]
    fn switcher_prefers_word_starts() {
        let (_d, v) = vault(&[("Ominous Mansion.md", ""), ("Meeting Notes.md", "")]);
        assert_eq!(v.switcher("mn", 5)[0], "Meeting Notes.md");
    }

    #[test]
    fn switcher_needs_a_subsequence() {
        let (_d, v) = vault(&[("abc.md", "")]);
        assert!(v.switcher("cba", 5).is_empty());
    }

    #[test]
    fn switcher_matches_aliases() {
        let (_d, v) = vault(&[("zzz.md", "---\naliases: [Project Hermes]\n---\n")]);
        assert_eq!(v.switcher("hermes", 5), ["zzz.md"]);
    }

    #[test]
    fn switcher_exact_title_first() {
        let (_d, v) = vault(&[("Rust Notes.md", ""), ("Rust.md", "")]);
        assert_eq!(v.switcher("rust", 5)[0], "Rust.md");
    }

    #[test]
    fn empty_switcher_lists_newest_first() {
        let (_d, mut v) = vault(&[("old.md", ""), ("new.md", "")]);
        v.notes[0].modified = 100;
        v.notes[1].modified = 50;
        assert_eq!(v.switcher("", 1), ["new.md"]);
    }

    #[test]
    fn guard_refuses_escapes() {
        for bad in ["", "/etc/passwd", "\\windows", "C:/x.md", "C:x.md", "..", "../x.md", "a/../../x.md", "a\\..\\..\\x.md", "./a.md", "a//b.md", "a.md:stream", "\\\\server\\share\\x.md", ".obsidian/app.json", ".trash/a.md", "a/.git/config", "a.. /x.md", "x.md.", "NUL", "con.md", "a/COM1.txt", "a\0.md"] {
            assert!(clean_rel(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn guard_accepts_and_normalises() {
        assert_eq!(clean_rel("a b\\c.d.md").expect("clean"), "a b/c.d.md");
        assert_eq!(clean_rel("console.md").expect("clean"), "console.md");
        assert_eq!(clean_rel("%2e%2e/x.md").expect("clean"), "%2e%2e/x.md");
    }

    #[test]
    fn every_entry_point_passes_the_guard() {
        let (dir, mut v) = vault(&[("a.md", "x")]);
        let outside = dir.path().parent().expect("parent").join("olympus-vault-escape.md");
        assert!(v.read("../olympus-vault-escape.md").is_err());
        assert!(v.write("../olympus-vault-escape.md", "x").is_err());
        assert!(v.create("../olympus-vault-escape.md", "x").is_err());
        assert!(v.delete("../a.md").is_err());
        assert!(v.refresh("../a.md").is_err());
        assert!(v.rename("a.md", "../olympus-vault-escape.md").is_err());
        assert!(v.rename("../a.md", "b.md").is_err());
        assert!(!outside.exists());
        assert!(dir.path().join("a.md").exists());
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_out_of_the_vault_is_refused() {
        let (dir, mut v) = vault(&[("a.md", "x")]);
        let outside = tempfile::tempdir().expect("temp dir");
        fs::write(outside.path().join("secret.md"), "secret").expect("write");
        let link = dir.path().join("door");
        let made = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(&link).arg(outside.path()).output().expect("mklink");
        assert!(made.status.success());
        v.rescan().expect("rescan");
        assert_eq!(v.notes().len(), 1);
        assert!(v.read("door/secret.md").is_err());
        assert!(v.write("door/new.md", "x").is_err());
        assert!(!outside.path().join("new.md").exists());
    }

    #[test]
    fn open_needs_a_real_folder() {
        assert!(Vault::open(Path::new("definitely/not/here")).is_err());
    }

    #[test]
    fn scan_skips_dot_folders_and_recurses() {
        let (_d, v) = vault(&[("a.md", ""), ("x/y/b.MD", ""), (".obsidian/c.md", ""), ("x/.hidden/d.md", ""), (".trash/e.md", ""), ("x/p.png", "z")]);
        assert_eq!(v.notes().iter().map(|n| n.rel.as_str()).collect::<Vec<_>>(), ["a.md", "x/y/b.MD"]);
        assert_eq!(v.attachments(), ["x/p.png"]);
    }

    #[test]
    fn non_utf8_note_is_still_indexed() {
        let (dir, _v) = vault(&[]);
        fs::write(dir.path().join("bad.md"), b"[[a]] \xff\xfe").expect("write");
        let v = Vault::open(dir.path()).expect("open");
        assert_eq!(v.notes()[0].links[0].target, "a");
        assert!(v.read("bad.md").is_err());
    }

    #[test]
    fn write_then_read_updates_the_index() {
        let (_d, mut v) = vault(&[]);
        v.write("sub/n.md", "[[x]] #t").expect("write");
        assert_eq!(v.read("sub/n.md").expect("read"), "[[x]] #t");
        assert_eq!(v.note("sub/n.md").expect("note").tags, ["t"]);
    }

    #[test]
    fn write_leaves_no_temp_files() {
        let (dir, mut v) = vault(&[]);
        v.write("n.md", "a").expect("write");
        v.write("n.md", "b").expect("write");
        assert_eq!(fs::read_dir(dir.path()).expect("dir").count(), 1);
    }

    #[test]
    fn write_only_takes_markdown() {
        let (_d, mut v) = vault(&[]);
        assert!(v.write("x.exe", "a").is_err());
    }

    #[test]
    fn writing_with_other_casing_keeps_one_entry() {
        let (_d, mut v) = vault(&[("Note.md", "a")]);
        v.write("note.md", "b").expect("write");
        assert_eq!(v.notes().len(), 1);
        assert_eq!(v.read("Note.md").expect("read"), "b");
    }

    #[test]
    fn create_refuses_existing_notes_and_keeps_their_text() {
        let (_d, mut v) = vault(&[("a.md", "keep")]);
        assert!(v.create("a.md", "new").is_err());
        assert_eq!(v.read("a.md").expect("read"), "keep");
    }

    #[test]
    fn create_makes_folders_and_indexes() {
        let (_d, mut v) = vault(&[]);
        v.create("x/y/n.md", "hi").expect("create");
        assert!(v.note("x/y/n.md").is_some());
    }

    #[test]
    fn create_refuses_unlinkable_names() {
        let (_d, mut v) = vault(&[]);
        assert!(v.create("a#b.md", "").is_err());
    }

    #[test]
    fn delete_moves_to_the_trash() {
        let (dir, mut v) = vault(&[("x/a.md", "text")]);
        v.delete("x/a.md").expect("delete");
        assert!(v.note("x/a.md").is_none());
        assert_eq!(fs::read_to_string(dir.path().join(".trash/x/a.md")).expect("trash"), "text");
    }

    #[test]
    fn deleting_twice_keeps_both_copies() {
        let (dir, mut v) = vault(&[("a.md", "one")]);
        v.delete("a.md").expect("delete");
        v.write("a.md", "two").expect("write");
        v.delete("a.md").expect("delete");
        assert_eq!(fs::read_to_string(dir.path().join(".trash/a.md")).expect("first"), "one");
        assert_eq!(fs::read_to_string(dir.path().join(".trash/a 1.md")).expect("second"), "two");
    }

    #[test]
    fn delete_of_a_missing_file_fails() {
        let (_d, mut v) = vault(&[]);
        assert!(v.delete("no.md").is_err());
    }

    #[test]
    fn refresh_sees_added_edited_and_removed_files() {
        let (dir, mut v) = vault(&[("a.md", "")]);
        fs::write(dir.path().join("a.md"), "#new").expect("edit");
        fs::write(dir.path().join("b.md"), "").expect("add");
        fs::write(dir.path().join("p.png"), "x").expect("add");
        v.refresh("a.md").expect("refresh");
        v.refresh("b.md").expect("refresh");
        v.refresh("p.png").expect("refresh");
        assert_eq!(v.note("a.md").expect("a").tags, ["new"]);
        assert_eq!(v.resolve("b", "a.md").as_deref(), Some("b.md"));
        assert_eq!(v.attachments(), ["p.png"]);
        fs::remove_file(dir.path().join("b.md")).expect("remove");
        v.refresh("b.md").expect("refresh");
        assert!(v.note("b.md").is_none());
    }

    #[test]
    fn refresh_handles_a_removed_folder() {
        let (dir, mut v) = vault(&[("x/a.md", ""), ("b.md", "")]);
        fs::remove_dir_all(dir.path().join("x")).expect("remove");
        v.refresh("x").expect("refresh");
        assert_eq!(v.notes().len(), 1);
    }

    #[test]
    fn rename_rewrites_every_form_of_link() {
        let (_d, mut v) = vault(&[
            ("old.md", "body"),
            ("b.md", "---\nup: \"[[old]]\"\n---\n[[old]] [[old.md]] [[old|name]] [[OLD#h]] [[old#^b]] ![[old]] [x](old.md) `[[old]]` [[other]]\n"),
            ("other.md", ""),
        ]);
        let changed = v.rename("old.md", "new.md").expect("rename");
        assert_eq!(changed, 8);
        assert_eq!(v.read("b.md").expect("read"), "---\nup: \"[[new]]\"\n---\n[[new]] [[new.md]] [[new|name]] [[new#h]] [[new#^b]] ![[new]] [x](new.md) `[[old]]` [[other]]\n");
        assert!(v.note("old.md").is_none());
        assert_eq!(v.read("new.md").expect("read"), "body");
        assert_eq!(v.backlinks("new.md").len(), 8);
    }

    #[test]
    fn rename_into_a_folder_uses_relative_markdown_links() {
        let (_d, mut v) = vault(&[("a b.md", ""), ("x/c.md", "[t](../a%20b.md) [[a b]]")]);
        assert_eq!(v.rename("a b.md", "y/z w.md").expect("rename"), 2);
        assert_eq!(v.read("x/c.md").expect("read"), "[t](../y/z%20w.md) [[z w]]");
    }

    #[test]
    fn rename_keeps_a_path_style_link_as_a_path() {
        let (_d, mut v) = vault(&[("x/a.md", ""), ("b.md", "[[x/a]]")]);
        v.rename("x/a.md", "y/a2.md").expect("rename");
        assert_eq!(v.read("b.md").expect("read"), "[[y/a2]]");
    }

    #[test]
    fn rename_uses_the_full_path_when_the_short_name_is_taken() {
        let (_d, mut v) = vault(&[("a.md", ""), ("c.md", "[[a]]"), ("q/w/new.md", ""), ("q/w/d.md", "[[a]]")]);
        v.rename("a.md", "r/new.md").expect("rename");
        assert_eq!(v.read("q/w/d.md").expect("read"), "[[r/new]]");
        assert_eq!(v.read("c.md").expect("read"), "[[new]]");
    }

    #[test]
    fn rename_leaves_a_link_that_still_resolves() {
        let (_d, mut v) = vault(&[("a.md", ""), ("b.md", "[[a]]")]);
        assert_eq!(v.rename("a.md", "sub/a.md").expect("rename"), 0);
        assert_eq!(v.backlinks("sub/a.md").len(), 1);
    }

    #[test]
    fn rename_leaves_alias_links_alone() {
        let (_d, mut v) = vault(&[("a.md", "---\naliases: [Nick]\n---\n"), ("b.md", "[[Nick]]")]);
        assert_eq!(v.rename("a.md", "z.md").expect("rename"), 0);
        assert_eq!(v.read("b.md").expect("read"), "[[Nick]]");
        assert_eq!(v.resolve("Nick", "b.md").as_deref(), Some("z.md"));
    }

    #[test]
    fn rename_does_not_steal_links_to_other_notes() {
        let (_d, mut v) = vault(&[("a.md", ""), ("z/b.md", ""), ("c.md", "[[b]]")]);
        assert_eq!(v.rename("a.md", "b.md").expect("rename"), 1);
        assert_eq!(v.read("c.md").expect("read"), "[[z/b]]");
        assert_eq!(v.backlinks("z/b.md").len(), 1);
    }

    #[test]
    fn moved_note_keeps_its_own_links_working() {
        let (_d, mut v) = vault(&[("x/a.md", "[up](../t.md) [[#h]] [self](a.md)"), ("t.md", "")]);
        v.rename("x/a.md", "p/q/a.md").expect("rename");
        assert_eq!(v.read("p/q/a.md").expect("read"), "[up](../../t.md) [[#h]] [self](a.md)");
    }

    #[test]
    fn rename_handles_attachments() {
        let (_d, mut v) = vault(&[("p.png", "x"), ("n.md", "![[p.png]]")]);
        assert_eq!(v.rename("p.png", "q.png").expect("rename"), 1);
        assert_eq!(v.read("n.md").expect("read"), "![[q.png]]");
    }

    #[test]
    fn rename_refuses_to_overwrite() {
        let (_d, mut v) = vault(&[("a.md", "A"), ("b.md", "B")]);
        assert!(v.rename("a.md", "b.md").is_err());
        assert_eq!(v.read("a.md").expect("read"), "A");
        assert_eq!(v.read("b.md").expect("read"), "B");
    }

    #[test]
    fn rename_refuses_a_changed_extension() {
        let (_d, mut v) = vault(&[("a.md", "A")]);
        assert!(v.rename("a.md", "a.exe").is_err());
    }

    #[test]
    fn rename_only_changing_case_works() {
        let (dir, mut v) = vault(&[("a.md", "A"), ("b.md", "[[a.md]]")]);
        v.rename("a.md", "A.md").expect("rename");
        assert_eq!(fs::read_dir(dir.path()).expect("dir").count(), 2);
        assert!(v.note("A.md").is_some());
    }

    #[test]
    fn rename_sees_edits_made_outside() {
        let (dir, mut v) = vault(&[("a.md", ""), ("b.md", "")]);
        fs::write(dir.path().join("b.md"), "fresh [[a]]").expect("edit");
        v.rename("a.md", "c.md").expect("rename");
        assert_eq!(v.read("b.md").expect("read"), "fresh [[c]]");
    }

    #[test]
    fn rename_refuses_to_rewrite_a_non_utf8_note() {
        let (dir, _v) = vault(&[("a.md", "A")]);
        fs::write(dir.path().join("bad.md"), b"[[a]] \xff").expect("write");
        let mut v = Vault::open(dir.path()).expect("open");
        assert!(v.rename("a.md", "c.md").is_err());
        assert!(dir.path().join("a.md").exists());
        assert_eq!(fs::read(dir.path().join("bad.md")).expect("read"), b"[[a]] \xff");
    }

    #[test]
    fn daily_path() {
        assert_eq!(daily_rel(NaiveDate::from_ymd_opt(2026, 3, 4).expect("date")), "Daily/2026-03-04.md");
    }

    #[test]
    fn template_defaults() {
        assert_eq!(apply_template("{{title}} {{date}} {{time}}", "T", time(2026, 3, 4, 15, 7)), "T 2026-03-04 15:07");
    }

    #[test]
    fn template_custom_formats() {
        assert_eq!(apply_template("{{date:dddd, MMMM Do YYYY}} {{time:h:mm A}}", "", time(2026, 11, 7, 15, 7)), "Saturday, November 7th 2026 3:07 PM");
    }

    #[test]
    fn template_literal_and_short_tokens() {
        assert_eq!(apply_template("{{date:[Week] WW, D/M/YY ddd MMM}}", "", time(2026, 3, 4, 1, 2)), "Week 10, 4/3/26 Wed Mar");
    }

    #[test]
    fn template_leaves_unknown_and_broken_markers() {
        assert_eq!(apply_template("{{other}} {{title", "T", time(2026, 1, 1, 0, 0)), "{{other}} {{title");
    }

    #[test]
    fn template_does_not_expand_the_title_again() {
        assert_eq!(apply_template("{{title}}", "{{date}}", time(2026, 1, 1, 0, 0)), "{{date}}");
    }

    #[test]
    fn ordinals() {
        let at = |d| format_moment("Do", time(2026, 1, d, 0, 0));
        assert_eq!([at(1), at(2), at(3), at(4), at(11), at(12), at(13), at(21), at(22), at(23)], ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "23rd"]);
    }

    fn graph_vault() -> (TempDir, Vault) {
        vault(&[
            ("a.md", "[[b]] [[b]] [[a]] [[gone]] ![[p.png]] #Tag"),
            ("b.md", "[[a]] [[c]] #tag"),
            ("c.md", "[[d]]"),
            ("d.md", ""),
            ("lonely.md", ""),
            ("p.png", "x"),
            ("q.png", "x"),
        ])
    }

    fn ids(data: &GraphData) -> Vec<&str> {
        data.nodes.iter().map(|n| n.id.as_str()).collect()
    }

    #[test]
    fn graph_has_notes_and_unresolved_by_default() {
        let (_d, v) = graph_vault();
        assert_eq!(ids(&v.graph(&GraphFilter::default())), ["a.md", "b.md", "c.md", "d.md", "lonely.md", "unresolved:gone"]);
    }

    #[test]
    fn graph_edges_are_deduplicated_and_have_no_self_loops() {
        let (_d, v) = graph_vault();
        let data = v.graph(&GraphFilter::default());
        assert_eq!(data.edges, [(0, 1), (0, 5), (1, 2), (2, 3)]);
        assert_eq!(data.nodes[0].degree, 2);
        assert_eq!(data.nodes[1].degree, 2);
    }

    #[test]
    fn graph_can_hide_orphans_and_unresolved() {
        let (_d, v) = graph_vault();
        let data = v.graph(&GraphFilter { show_orphans: false, hide_unresolved: true, ..GraphFilter::default() });
        assert_eq!(ids(&data), ["a.md", "b.md", "c.md", "d.md"]);
    }

    #[test]
    fn graph_shows_tags_as_one_node_per_identity() {
        let (_d, v) = graph_vault();
        let data = v.graph(&GraphFilter { show_tags: true, hide_unresolved: true, show_orphans: false, ..GraphFilter::default() });
        let tag = data.nodes.iter().find(|n| n.kind == NodeKind::Tag).expect("tag");
        assert_eq!((tag.label.as_str(), tag.degree), ("#Tag", 2));
    }

    #[test]
    fn graph_shows_attachments_including_unreferenced_ones() {
        let (_d, v) = graph_vault();
        let data = v.graph(&GraphFilter { show_attachments: true, ..GraphFilter::default() });
        let found: Vec<(&str, usize)> = data.nodes.iter().filter(|n| n.kind == NodeKind::Attachment).map(|n| (n.id.as_str(), n.degree)).collect();
        assert_eq!(found, [("p.png", 1), ("q.png", 0)]);
    }

    #[test]
    fn graph_query_filters_notes_and_their_edges() {
        let (_d, v) = graph_vault();
        let data = v.graph(&GraphFilter { query: "file:a.md OR file:d.md".into(), hide_unresolved: true, ..GraphFilter::default() });
        assert_eq!(ids(&data), ["a.md", "d.md"]);
        assert!(data.edges.is_empty());
    }

    #[test]
    fn graph_groups_take_the_first_match() {
        let (_d, v) = graph_vault();
        let data = v.graph(&GraphFilter { groups: vec!["tag:tag".into(), "file:a".into(), "file:c".into()], ..GraphFilter::default() });
        let groups: Vec<Option<usize>> = data.nodes.iter().map(|n| n.group).collect();
        assert_eq!(groups, [Some(0), Some(0), Some(2), None, None, None]);
    }

    #[test]
    fn local_graph_stops_at_its_depth() {
        let (_d, v) = graph_vault();
        let filter = |depth| GraphFilter { local: Some(("c.md".into(), depth)), hide_unresolved: true, ..GraphFilter::default() };
        assert_eq!(ids(&v.graph(&filter(0))), ["c.md"]);
        assert_eq!(ids(&v.graph(&filter(1))), ["b.md", "c.md", "d.md"]);
        assert_eq!(ids(&v.graph(&filter(2))), ["a.md", "b.md", "c.md", "d.md"]);
    }

    #[test]
    fn local_graph_keeps_its_root_when_orphans_are_hidden() {
        let (_d, v) = graph_vault();
        let data = v.graph(&GraphFilter { local: Some(("lonely.md".into(), 2)), show_orphans: false, ..GraphFilter::default() });
        assert_eq!(ids(&data), ["lonely.md"]);
    }

    #[test]
    fn local_graph_of_a_missing_note_is_empty() {
        let (_d, v) = graph_vault();
        assert!(v.graph(&GraphFilter { local: Some(("no.md".into(), 2)), ..GraphFilter::default() }).nodes.is_empty());
    }
}
