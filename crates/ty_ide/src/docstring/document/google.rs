//! Parsing for Google-style docstring sections.
//!
//! The [Google Python Style Guide](https://google.github.io/styleguide/pyguide.html#s3.8.3-functions-and-methods)
//! describes the canonical conventions but does not define a formal grammar. This parser recognizes
//! these parameter section headings:
//!
//! - `Args`, `Arguments`, and `Parameters`
//! - `Keyword Args` and `Keyword Arguments`
//! - `Other Args`, `Other Arguments`, and `Other Parameters`
//!
//! It accepts comma-separated Python names with optional parenthesized types, preserves
//! continuation text, and skips section-like text inside preformatted or container blocks. Other
//! known headings only delimit parameter sections; their contents are not parsed here.
//!
//! Example:
//!
//! ```text
//! Copy a file with retry controls.
//!
//! Args:
//!     source (str): Path to copy.
//!     destination: Destination path.
//!
//! Keyword Args:
//!     timeout, deadline (float): Time limits in seconds.
//!
//! Other Parameters:
//!     retries: Number of retries.
//! ```

use indexmap::IndexMap;
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_text_size::{TextRange, TextSize};

use super::SectionKind;
use super::preformatted::PreformattedBlockScanner;
use super::syntax::{
    ParsedLine, indentation, parsed_lines, split_once_at_top_level_colon,
    split_trailing_parenthesized_group, starts_container_block,
};

/// Returns parameter documentation from recognized Google-style parameter sections.
///
/// `normalized_source` must have already undergone PEP-257 trimming and universal newline
/// normalization.
pub(super) fn parameter_documentation(normalized_source: &str) -> IndexMap<String, String> {
    let mut parameters = Parameters::default();
    for section in sections(normalized_source) {
        let (kind, _, fragments) = section.into_parts();
        if matches!(
            kind,
            SectionKind::Parameters | SectionKind::KeywordArguments | SectionKind::OtherParameters
        ) {
            parameters.extend_fragments(fragments);
        }
    }
    parameters.into_inner()
}

/// Returns recognized Google-style sections in source order.
///
/// `source` must have already undergone PEP-257 trimming and universal
/// newline normalization (typically via `docstring::documentation_trim`).
pub(in crate::docstring) fn sections(source: &str) -> impl Iterator<Item = Section> {
    let lines = parsed_lines(source);
    Parser::new().parse(&lines).into_iter()
}

/// A recognized Google-style docstring section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::docstring) struct Section {
    kind: SectionKind,
    range: TextRange,
    body: SectionBody,
}

impl Section {
    /// Consumes this section and returns its canonical kind, source range, and body.
    fn into_parts(self) -> (SectionKind, TextRange, Vec<BodyFragment>) {
        (self.kind, self.range, self.body.fragments)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SectionBody {
    fragments: Vec<BodyFragment>,
    /// Whether the structured Markdown renderer can represent the body without changing its
    /// meaning.
    is_renderable: bool,
}

impl SectionBody {
    /// Creates a renderable body containing the description as a single prose fragment.
    fn from_prose(description: String) -> Self {
        let fragments = (!description.is_empty())
            .then_some(BodyFragment::Prose(description))
            .into_iter()
            .collect();
        Self {
            fragments,
            is_renderable: true,
        }
    }

    /// Creates an unrenderable body for a section whose contents remain opaque.
    fn opaque() -> Self {
        Self {
            fragments: Vec::new(),
            is_renderable: false,
        }
    }
}

/// One parsed fragment in a Google section body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::docstring) enum BodyFragment {
    /// Section-level prose that is not attached to a named item.
    Prose(String),
    /// A named item and its description.
    Item(Item),
}

/// A named item in a Google section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::docstring) struct Item {
    display_name: String,
    ty: Option<String>,
    description: String,
}

impl Item {
    /// Consumes this item and returns its display parts.
    pub(in crate::docstring) fn into_parts(self) -> (String, Option<String>, String) {
        (self.display_name, self.ty, self.description)
    }
}

/// Splits a display name from a balanced trailing parenthesized type.
fn split_name_and_type(value: &str) -> (&str, Option<&str>) {
    let Some((name, ty)) = split_trailing_parenthesized_group(value) else {
        return (value, None);
    };
    let name = name.trim();
    let ty = ty.trim();

    if name.is_empty() || ty.is_empty() {
        (value, None)
    } else {
        (name, Some(ty))
    }
}

/// Returns whether `name` is a valid Python parameter name, including variadic prefixes.
fn is_parameter_name(name: &str) -> bool {
    let identifier = name.strip_prefix('*').unwrap_or(name);
    let identifier = identifier.strip_prefix('*').unwrap_or(identifier);
    is_identifier(identifier)
}

#[derive(Default)]
struct Parameters(IndexMap<String, String>);

impl Parameters {
    fn extend_fragments(&mut self, fragments: Vec<BodyFragment>) {
        for fragment in fragments {
            let BodyFragment::Item(item) = fragment else {
                continue;
            };
            let (display_name, _, description) = item.into_parts();
            let description = parameter_description(&description);
            if description.is_empty() {
                continue;
            }
            for name in display_name.split(',').map(str::trim) {
                self.0.insert(name.to_string(), description.clone());
            }
        }
    }

    fn into_inner(self) -> IndexMap<String, String> {
        self.0
    }
}

fn parameter_description(description: &str) -> String {
    description
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns whether every component of `name` is a Python identifier.
fn is_dotted_identifier(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(is_identifier)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    kind: HeaderKind,
    form: HeaderForm,
    indent: TextSize,
    range: TextRange,
}

impl Header {
    fn parse(line: ParsedLine<'_>) -> Option<Self> {
        let trimmed = line.text.trim();
        let (kind, form) = if let Some(name) = trimmed.strip_suffix(':') {
            (HeaderKind::from_name(name.trim())?, HeaderForm::Section)
        } else {
            if trimmed.ends_with("::") {
                return None;
            }
            let (name, description) = split_once_at_top_level_colon(trimmed)?;
            let name = name.trim();
            if description.trim().is_empty() || !name.chars().next().is_some_and(char::is_uppercase)
            {
                return None;
            }
            (HeaderKind::from_name(name)?, HeaderForm::Inline)
        };

        Some(Self {
            kind,
            form,
            indent: line.indent,
            range: line.range,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderForm {
    Section,
    Inline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderKind {
    Structured(SectionKind),
    Opaque,
}

impl HeaderKind {
    fn from_name(name: &str) -> Option<Self> {
        let normalized = name
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        Some(match normalized.as_str() {
            "args" | "arguments" | "parameters" => Self::Structured(SectionKind::Parameters),
            "keyword args" | "keyword arguments" => Self::Structured(SectionKind::KeywordArguments),
            "other args" | "other arguments" | "other parameters" => {
                Self::Structured(SectionKind::OtherParameters)
            }
            "attributes" => Self::Structured(SectionKind::Attributes),
            "return" | "returns" => Self::Structured(SectionKind::Returns),
            "yield" | "yields" => Self::Structured(SectionKind::Yields),
            "raise" | "raises" => Self::Structured(SectionKind::Raises),
            "attention" | "caution" | "danger" | "error" | "example" | "examples" | "hint"
            | "important" | "methods" | "note" | "notes" | "references" | "see also" | "tip"
            | "todo" | "todos" | "warning" | "warnings" | "warns" => Self::Opaque,
            _ => return None,
        })
    }
}

struct Parser<'a> {
    outside_preformatted: PreformattedBlockScanner<'a>,
    outside_container: ContainerBlockScanner,
    current: Option<SectionBuilder<'a>>,
    sections: Vec<Section>,
}

impl<'a> Parser<'a> {
    fn new() -> Self {
        Self {
            outside_preformatted: PreformattedBlockScanner::default(),
            outside_container: ContainerBlockScanner::default(),
            current: None,
            sections: Vec::new(),
        }
    }

    fn parse(mut self, lines: &[ParsedLine<'a>]) -> Vec<Section> {
        for line in lines.iter().copied() {
            self.push_line(line);
        }
        self.finish_current();
        self.sections
    }

    fn push_line(&mut self, line: ParsedLine<'a>) {
        let line_header = (!line.text.trim().is_empty())
            .then(|| Header::parse(line))
            .flatten();

        if let Some(mut section) = self.current.take() {
            if section.push_line(line, line_header) {
                self.current = Some(section);
                return;
            }
            self.finish(section);
        }

        if self
            .outside_preformatted
            .consume_preformatted_line(line.text)
            || self.outside_container.consume(line)
        {
            return;
        }

        if let Some(header) = line_header
            && header.form == HeaderForm::Section
        {
            self.current = Some(SectionBuilder::new(header));
        } else {
            self.outside_preformatted
                .observe_line_outside_preformatted_block(line.text);
        }
    }

    fn finish_current(&mut self) {
        if let Some(section) = self.current.take() {
            self.finish(section);
        }
    }

    fn finish(&mut self, section: SectionBuilder<'a>) {
        if let Some(section) = section.finish() {
            self.sections.push(section);
        }
    }
}

#[derive(Default)]
struct ContainerBlockScanner {
    indent: Option<TextSize>,
}

impl ContainerBlockScanner {
    fn consume(&mut self, line: ParsedLine<'_>) -> bool {
        if let Some(indent) = self.indent {
            if line.text.trim().is_empty() || line.indent > indent {
                return true;
            }
            self.indent = None;
        }

        if starts_container_block(line.text) {
            self.indent = Some(line.indent);
            true
        } else {
            false
        }
    }
}

struct SectionBuilder<'a> {
    section_header: Header,
    range: TextRange,
    /// Blank lines whose ownership depends on the next nonblank line.
    pending_blank_lines: Vec<ParsedLine<'a>>,
    /// Prevents code examples from participating in section-boundary detection.
    preformatted: PreformattedBlockScanner<'a>,
    /// Indentation established by the first item-like line.
    ///
    /// This controls section boundaries and may come from a line that cannot be represented as a
    /// structured item.
    boundary_item_indent: Option<TextSize>,
    body: BodyBuilder<'a>,
}

impl<'a> SectionBuilder<'a> {
    fn new(section_header: Header) -> Self {
        Self {
            range: section_header.range,
            pending_blank_lines: Vec::new(),
            preformatted: PreformattedBlockScanner::default(),
            boundary_item_indent: None,
            body: BodyBuilder::new(section_header.kind),
            section_header,
        }
    }

    /// Returns `false` when `line` belongs outside this section.
    fn push_line(&mut self, line: ParsedLine<'a>, line_header: Option<Header>) -> bool {
        if self.preformatted.is_active() && self.preformatted.consume_preformatted_line(line.text) {
            self.commit_pending_blank_lines();
            self.push_content_line(line, ItemLine::default());
            return true;
        }

        if line.text.trim().is_empty() {
            self.pending_blank_lines.push(line);
            return true;
        }

        let also_parses_as_section_header = line_header.is_some()
            && line
                .text
                .trim()
                .chars()
                .next()
                .is_some_and(char::is_uppercase);
        let item_line = ItemLine::classify(
            self.section_header.kind,
            line,
            also_parses_as_section_header,
        );
        let has_leading_blank_lines = !self.pending_blank_lines.is_empty();

        if self.ends_before(
            line,
            line_header,
            item_line.boundary_item,
            has_leading_blank_lines,
        ) {
            return false;
        }

        self.commit_pending_blank_lines();
        if item_line.boundary_item {
            self.boundary_item_indent.get_or_insert(line.indent);
        }
        self.push_content_line(line, item_line);

        if !self.preformatted.consume_preformatted_line(line.text) {
            self.preformatted
                .observe_line_outside_preformatted_block(line.text);
        }
        true
    }

    fn ends_before(
        &self,
        line: ParsedLine<'_>,
        line_header: Option<Header>,
        boundary_item: bool,
        has_leading_blank_lines: bool,
    ) -> bool {
        // A sibling-level recognized header starts a new section.
        if line_header.is_some_and(|header| header.indent <= self.section_header.indent) {
            return true;
        }

        // Without an item indentation, blank-separated prose belongs outside a return section.
        if has_leading_blank_lines
            && line.indent <= self.section_header.indent
            && self.boundary_item_indent.is_none()
            && matches!(
                self.section_header.kind,
                HeaderKind::Structured(SectionKind::Returns | SectionKind::Yields)
            )
        {
            return true;
        }

        // Blank-separated aligned prose ends a parameter section unless it starts another item.
        if has_leading_blank_lines
            && self.section_header.kind.is_parameter_section()
            && self.boundary_item_indent == Some(line.indent)
            && !boundary_item
        {
            return true;
        }

        match line.indent.cmp(&self.section_header.indent) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => {
                let item_indent_matches = self
                    .boundary_item_indent
                    .is_none_or(|indent| indent == line.indent);
                !item_indent_matches
                    || (!self.section_header.kind.is_parameter_section() && !boundary_item)
            }
        }
    }

    fn commit_pending_blank_lines(&mut self) {
        for line in self.pending_blank_lines.drain(..) {
            self.range = self.range.cover(line.range);
            self.body.push_blank_line();
        }
    }

    fn push_content_line(&mut self, line: ParsedLine<'a>, item_line: ItemLine<'a>) {
        self.range = self.range.cover(line.range);
        self.body.push_line(self.section_header, line, item_line);
    }

    fn finish(self) -> Option<Section> {
        let HeaderKind::Structured(kind) = self.section_header.kind else {
            return None;
        };
        let body = self.body.finish();
        Some(Section {
            kind,
            range: self.range,
            body,
        })
    }
}

enum BodyBuilder<'a> {
    /// A section whose body consists of named items and their descriptions.
    ItemList(ItemListBuilder<'a>),
    /// A section whose entire body is semantic prose (e.g., returns or yields).
    Prose(DescriptionBuilder<'a>),
    /// A recognized section that participates in boundary detection but is not rendered.
    Opaque,
}

impl<'a> BodyBuilder<'a> {
    fn new(kind: HeaderKind) -> Self {
        match kind {
            HeaderKind::Structured(SectionKind::Returns | SectionKind::Yields) => {
                Self::Prose(DescriptionBuilder::default())
            }
            HeaderKind::Structured(_) => Self::ItemList(ItemListBuilder::default()),
            HeaderKind::Opaque => Self::Opaque,
        }
    }

    fn push_blank_line(&mut self) {
        match self {
            Self::ItemList(body) => body.push_blank_line(),
            Self::Prose(description) => description.push_continuation(""),
            Self::Opaque => {}
        }
    }

    fn push_line(&mut self, section_header: Header, line: ParsedLine<'a>, item_line: ItemLine<'a>) {
        match self {
            Self::ItemList(builder) => builder.push_line(section_header, line, item_line),
            Self::Prose(builder) => builder.push_line(line.text),
            Self::Opaque => {}
        }
    }

    fn finish(self) -> SectionBody {
        match self {
            Self::ItemList(builder) => builder.finish(),
            Self::Prose(builder) => SectionBody::from_prose(builder.finish()),
            Self::Opaque => SectionBody::opaque(),
        }
    }
}

struct ItemListBuilder<'a> {
    fragments: Vec<BodyFragment>,
    current_item: Option<ItemBuilder<'a>>,
    /// Content encountered before the first recognized item.
    leading_prose: DescriptionBuilder<'a>,
    /// Indentation established by the first renderable item.
    item_indent: Option<TextSize>,
    /// Whether every line seen so far can be rendered faithfully as Markdown.
    is_renderable: bool,
}

impl Default for ItemListBuilder<'_> {
    /// Creates an empty builder that is renderable by default (meaning that we
    /// must actually encounter an unrenderable line in order to prevent
    /// rendering an item list as Markdown).
    fn default() -> Self {
        Self {
            fragments: Vec::new(),
            current_item: None,
            leading_prose: DescriptionBuilder::default(),
            item_indent: None,
            is_renderable: true,
        }
    }
}

impl<'a> ItemListBuilder<'a> {
    fn push_blank_line(&mut self) {
        if let Some(item) = &mut self.current_item {
            item.description.push_continuation("");
        } else {
            self.leading_prose.push_continuation("");
        }
    }

    fn push_line(&mut self, section_header: Header, line: ParsedLine<'a>, item_line: ItemLine<'a>) {
        let line_indent = indentation(line.text);
        if self
            .item_indent
            .is_none_or(|item_indent| line_indent == item_indent)
            && let Some(item_header) = item_line.item_header
        {
            self.finish_leading_prose();
            self.finish_current_item();
            self.current_item = Some(ItemBuilder::new(&item_header));
            self.item_indent.get_or_insert(line_indent);
            self.is_renderable &= !item_line.also_parses_as_section_header;
            return;
        }

        if let Some(item_indent) = self.item_indent
            && !item_line.can_render_as_continuation(section_header.kind, line_indent, item_indent)
        {
            self.is_renderable = false;
        }

        if let Some(item) = &mut self.current_item {
            item.description.push_continuation(line.text);
        } else {
            self.is_renderable = false;
            self.leading_prose.push_line(line.text);
        }
    }

    fn finish_leading_prose(&mut self) {
        let prose = std::mem::take(&mut self.leading_prose).finish();
        if !prose.is_empty() {
            self.fragments.push(BodyFragment::Prose(prose));
        }
    }

    fn finish_current_item(&mut self) {
        if let Some(item) = self.current_item.take() {
            self.fragments.push(BodyFragment::Item(item.finish()));
        }
    }

    fn finish(mut self) -> SectionBody {
        self.finish_leading_prose();
        self.finish_current_item();
        SectionBody {
            fragments: self.fragments,
            is_renderable: self.is_renderable,
        }
    }
}

struct ItemBuilder<'a> {
    display_name: &'a str,
    ty: Option<&'a str>,
    description: DescriptionBuilder<'a>,
}

impl<'a> ItemBuilder<'a> {
    fn new(item_header: &ItemHeader<'a>) -> Self {
        Self {
            display_name: item_header.display_name,
            ty: item_header.ty,
            description: DescriptionBuilder::with_inline(item_header.inline_description),
        }
    }

    fn finish(self) -> Item {
        Item {
            display_name: self.display_name.to_string(),
            ty: self.ty.map(str::to_string),
            description: self.description.finish(),
        }
    }
}

#[derive(Default)]
struct DescriptionBuilder<'a> {
    inline: Option<&'a str>,
    continuation_lines: Vec<&'a str>,
}

impl<'a> DescriptionBuilder<'a> {
    fn with_inline(inline: &'a str) -> Self {
        let inline = inline.trim();
        Self {
            inline: (!inline.is_empty()).then_some(inline),
            continuation_lines: Vec::new(),
        }
    }

    fn push_line(&mut self, line: &'a str) {
        if self.inline.is_none() && self.continuation_lines.is_empty() {
            self.inline = Some(line.trim());
        } else {
            self.push_continuation(line);
        }
    }

    fn push_continuation(&mut self, line: &'a str) {
        self.continuation_lines.push(line);
    }

    fn finish(self) -> String {
        let continuation_indent = self
            .continuation_lines
            .iter()
            .filter(|line| !line.trim().is_empty())
            .map(|line| indentation(line))
            .min()
            .unwrap_or_default();

        let mut lines =
            Vec::with_capacity(self.continuation_lines.len() + usize::from(self.inline.is_some()));
        if let Some(inline) = self.inline {
            lines.push(inline.to_string());
        }
        lines.extend(self.continuation_lines.into_iter().map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                strip_indentation(line, continuation_indent)
                    .trim_end()
                    .to_string()
            }
        }));

        let Some(start) = lines.iter().position(|line| !line.is_empty()) else {
            return String::new();
        };
        let end = lines
            .iter()
            .rposition(|line| !line.is_empty())
            .map_or(start, |index| index + 1);
        lines[start..end].join("\n")
    }
}

#[derive(Default)]
struct ItemLine<'a> {
    /// Whether this line establishes item indentation for section-boundary detection.
    boundary_item: bool,
    item_header: Option<ItemHeader<'a>>,
    /// Whether this line resembles an item but is actually a URL or path continuation.
    is_item_like_continuation: bool,
    /// Whether this item could instead introduce a new section.
    also_parses_as_section_header: bool,
}

impl<'a> ItemLine<'a> {
    fn can_render_as_continuation(
        &self,
        section_kind: HeaderKind,
        line_indent: TextSize,
        item_indent: TextSize,
    ) -> bool {
        // More deeply indented lines are unambiguously part of the current item.
        line_indent > item_indent
            // Although the style guide suggests indenting continuation lines,
            // aligned parameter prose is common in practice.
            || (line_indent == item_indent && section_kind.is_parameter_section())
            // Aligned URLs and paths are continuations despite resembling item headers.
            || (line_indent == item_indent && self.is_item_like_continuation)
    }

    fn classify(
        section_kind: HeaderKind,
        line: ParsedLine<'a>,
        also_parses_as_section_header: bool,
    ) -> Self {
        let HeaderKind::Structured(kind) = section_kind else {
            return Self::default();
        };
        if matches!(kind, SectionKind::Returns | SectionKind::Yields) {
            return Self {
                boundary_item: true,
                ..Self::default()
            };
        }

        let line_text = line.text.trim();
        let Some((raw_name, inline_description)) = split_field_colon(line_text) else {
            return Self::default();
        };
        let name = raw_name.trim();
        if name.is_empty() {
            return Self::default();
        }

        let (display_name, ty) = match kind {
            SectionKind::Parameters
            | SectionKind::KeywordArguments
            | SectionKind::OtherParameters => {
                let (display_name, ty) = split_name_and_type(name);
                if !is_parameter_display_name(display_name) {
                    return Self::default();
                }
                (display_name, ty)
            }
            SectionKind::Attributes => {
                let (display_name, ty) = split_name_and_type(name);
                if !is_attribute_display_name(display_name) {
                    return Self {
                        boundary_item: true,
                        ..Self::default()
                    };
                }
                (display_name, ty)
            }
            SectionKind::Raises => {
                if !is_dotted_identifier(name) {
                    return Self {
                        boundary_item: true,
                        ..Self::default()
                    };
                }
                (name, None)
            }
            SectionKind::Returns | SectionKind::Yields => return Self::default(),
        };

        // URLs (`https://...`), Windows paths (`C:\\...`), and reST literal-block introductions
        // (`Example::`) are description continuations rather than item headers.
        //
        // This was configured from a survey of such continuations in popular public projects that
        // use Google-style docstrings; it may need to be reconfigured in the future.
        if matches!(
            inline_description.as_bytes().first(),
            Some(b'/' | b'\\' | b':')
        ) {
            return Self {
                is_item_like_continuation: true,
                ..Self::default()
            };
        }

        Self {
            boundary_item: true,
            item_header: Some(ItemHeader {
                display_name,
                ty,
                inline_description,
            }),
            is_item_like_continuation: false,
            also_parses_as_section_header,
        }
    }
}

struct ItemHeader<'a> {
    display_name: &'a str,
    ty: Option<&'a str>,
    inline_description: &'a str,
}

fn split_field_colon(line: &str) -> Option<(&str, &str)> {
    let mut start = 0;
    while start < line.len() {
        let (before_colon, after_colon) = split_once_at_top_level_colon(line.get(start..)?)?;
        let colon = start + before_colon.len();
        if let Some(role_end) = rst_role_markup_end(line, colon) {
            start = role_end;
            continue;
        }
        return Some((&line[..colon], after_colon));
    }
    None
}

fn rst_role_markup_end(line: &str, start: usize) -> Option<usize> {
    let rest = line.get(start..)?;
    let after_initial_colon = rest.strip_prefix(':')?;
    let role_end = after_initial_colon.find(":`")?;
    let role = &after_initial_colon[..role_end];
    if role.is_empty()
        || !role
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || matches!(char, ':' | '_' | '-' | '.'))
    {
        return None;
    }

    let content_start = start + ':'.len_utf8() + role_end + ":`".len();
    let content = line.get(content_start..)?;
    let closing_backtick = content.find('`')?;
    Some(content_start + closing_backtick + '`'.len_utf8())
}

fn is_parameter_display_name(display_name: &str) -> bool {
    display_name
        .split(',')
        .all(|name| is_parameter_name(name.trim()))
}

fn is_attribute_display_name(display_name: &str) -> bool {
    display_name
        .split(',')
        .all(|name| is_dotted_identifier(name.trim()))
}

fn strip_indentation(line: &str, width: TextSize) -> &str {
    let mut indentation_width = TextSize::default();
    for (index, char) in line.char_indices() {
        let next_indentation_width = match char {
            ' ' => indentation_width + TextSize::new(1),
            '\t' => TextSize::new((indentation_width.to_u32() / 8 + 1) * 8),
            _ => return &line[index..],
        };

        if next_indentation_width > width {
            return &line[index..];
        }

        indentation_width = next_indentation_width;
        if indentation_width == width {
            return &line[index + char.len_utf8()..];
        }
    }

    ""
}

impl HeaderKind {
    fn is_parameter_section(self) -> bool {
        matches!(
            self,
            Self::Structured(
                SectionKind::Parameters
                    | SectionKind::KeywordArguments
                    | SectionKind::OtherParameters
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use itertools::Itertools;

    use super::{BodyFragment, Item, SectionKind, parameter_documentation, sections};

    #[test]
    fn extracts_aligned_parameter_items() {
        let raw = "\
Arguments:
first: First parameter.
Aligned continuation.
second: Second parameter.
Returns:
bool: Result.";

        assert_snapshot!(display_parameters(raw), @"
        first:
          │ First parameter.
          │ Aligned continuation.
        second:
          │ Second parameter.
        ");
    }

    #[test]
    fn uses_visual_indentation_for_parameter_items() {
        let raw = "\
Args:
  \tfirst: First parameter.
        second: Second parameter.";

        assert_snapshot!(display_parameters(raw), @"
        first:
          │ First parameter.
        second:
          │ Second parameter.
        ");
    }

    #[test]
    fn extracts_comma_separated_parameter_names() {
        let raw = "\
Args:
    x, y: Coordinates.";

        assert_snapshot!(display_parameters(raw), @"
        x:
          │ Coordinates.
        y:
          │ Coordinates.
        ");
    }

    #[test]
    fn ignores_prose_before_first_parameter() {
        let raw = "\
Args:
Partition into non-overlapping windows with padding if needed.
    hidden_states (tensor): Input tokens.";

        assert_snapshot!(display_parameters(raw), @"
        hidden_states:
          │ Input tokens.
        ");
    }

    #[test]
    fn extracts_parameter_with_unbalanced_type_brackets() {
        let raw = "\
Args:
    query_embeddings (`Union[torch.Tensor, list[torch.Tensor]`): Query embeddings.";

        assert_snapshot!(display_parameters(raw), @"
        query_embeddings:
          │ Query embeddings.
        ");
    }

    #[test]
    fn accepts_dashed_parameter_section_underline() {
        let raw = "\
Args:
----
    value: Parameter documentation.";

        assert_snapshot!(display_parameters(raw), @"
        value:
          │ Parameter documentation.
        ");
    }

    #[test]
    fn treats_invalid_parameter_name_as_continuation() {
        let raw = "\
Args:
    value: Initial documentation.
    value, for example: can be omitted.";

        assert_snapshot!(display_parameters(raw), @"
        value:
          │ Initial documentation.
          │ value, for example: can be omitted.
        ");
    }

    #[test]
    fn uses_last_documentation_for_duplicate_parameter() {
        let raw = "\
Args:
    value: First documentation.
    value: Replacement documentation.";

        assert_snapshot!(display_parameters(raw), @"
        value:
          │ Replacement documentation.
        ");
    }

    #[test]
    fn parses_parenthesized_type_with_quoted_parenthesis() {
        let raw = "\
Args:
    value (Literal[\"(\"]): Quoted parenthesis.";

        assert_snapshot!(display_parameters(raw), @"
        value:
          │ Quoted parenthesis.
        ");
    }

    #[test]
    fn ignores_callable_like_parameter_names() {
        let raw = "\
Args:
    callback() (Callable): Not a parameter.
    value: Documentation.";

        assert_snapshot!(display_parameters(raw), @"
        value:
          │ Documentation.
        ");
    }

    #[test]
    fn preserves_parameter_paragraph_breaks() {
        let raw = "\
Args:
    value: First paragraph.


        Second paragraph.";

        assert_snapshot!(display_parameters(raw), @"
        value:
          │ First paragraph.
          │
          │
          │ Second paragraph.
        ");
    }

    #[test]
    fn recognizes_parameter_section_headings() {
        for heading in [
            "Args",
            "Arguments",
            "Parameters",
            "Keyword Args",
            "Keyword Arguments",
            "Other Args",
            "Other Arguments",
            "Other Parameters",
        ] {
            let raw = format!(
                "\
{heading}:
    value: Parameter documentation."
            );
            assert_parameter_documentation(&raw, &[("value", "Parameter documentation.")]);
        }
    }

    #[test]
    fn ends_parameter_section_at_structured_section_header() {
        assert_parameter_documentation(
            "\
Args:
    value: Parameter documentation.
Methods:
    helper: Method documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn ends_unindented_parameter_section_at_aligned_prose() {
        assert_parameter_documentation(
            "\
Args:
value: Parameter documentation.

Additional details.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn ends_indented_parameter_section_at_aligned_prose() {
        assert_parameter_documentation(
            "\
Args:
    value: Parameter documentation.

    Additional details.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn ends_indented_parameter_section_at_inline_section_header() {
        assert_parameter_documentation(
            "\
Args:
    first: First parameter.
    last: Last parameter.

Returns: Result.",
            &[("first", "First parameter."), ("last", "Last parameter.")],
        );
    }

    #[test]
    fn ends_unindented_parameter_section_at_inline_section_header() {
        assert_parameter_documentation(
            "\
Args:
first: First parameter.
last: Last parameter.
Returns: Result.",
            &[("first", "First parameter."), ("last", "Last parameter.")],
        );
    }

    #[test]
    fn pep257_normalization_recognizes_shifted_sibling_section() {
        assert_parameter_documentation(
            "\
Note:
        context

    Args:
        value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn pep257_normalization_preserves_nested_section() {
        assert_parameter_documentation(
            "
    Note:
        context

        Args:
            nested: Not parameter documentation.",
            &[],
        );
    }

    #[test]
    fn pep257_normalization_ends_section_at_dedented_header() {
        assert_parameter_documentation(
            "\
Args:
        value: Parameter documentation.
    Returns:
        bool: Result.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn adjacent_aligned_section_headers_are_siblings() {
        assert_parameter_documentation(
            "\
Example:
Args:
    value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn finds_shifted_top_level_section() {
        assert_parameter_documentation(
            "\
A decoded newline follows:
This line starts at column zero.

    Keyword Args:
        shifted: Documentation in a shifted section.",
            &[("shifted", "Documentation in a shifted section.")],
        );
    }

    #[test]
    fn finds_parameter_section_after_first_line_literal_block() {
        // Regression for https://github.com/pytorch/pytorch/blob/e3f5bf0b18585511e6cd7d7a574ebf82f465e5ae/torch/_native/instrumentation.py#L365-L383
        assert_parameter_documentation(
            "\
Instrument a single ``@triton.jit`` kernel, stacked above the jit::

        @instrument_triton_kernel(\"aten::bmm\")
        @triton.jit
        def _bmm_kernel(...): ...

    A Triton kernel compiles lazily and caches variants on the kernel object.

    Args:
        op: Operator symbol being compiled for, e.g. ``\"aten::bmm\"``.",
            &[(
                "op",
                "Operator symbol being compiled for, e.g. ``\"aten::bmm\"``.",
            )],
        );
    }

    #[test]
    fn keeps_colon_prose_in_parameter_documentation() {
        assert_parameter_documentation(
            "\
Args:
    param1 (str): The first parameter description.
    For example: pass an absolute path.
    param2: The second parameter description.",
            &[
                (
                    "param1",
                    "The first parameter description.\nFor example: pass an absolute path.",
                ),
                ("param2", "The second parameter description."),
            ],
        );
    }

    #[test]
    fn keeps_rest_literal_blocks_in_parameter_documentation() {
        assert_parameter_documentation(
            "\
Args:
    value: Documentation.
        Example::
            Args:
                nested: Not parameter documentation.
    other: Other documentation.",
            &[
                (
                    "value",
                    "Documentation.\nExample::\nArgs:\nnested: Not parameter documentation.",
                ),
                ("other", "Other documentation."),
            ],
        );
    }

    #[test]
    fn extracts_variadic_parameters() {
        assert_parameter_documentation(
            "\
Args:
    *args: Extra positional arguments.
    **kwargs: Extra keyword arguments.",
            &[
                ("*args", "Extra positional arguments."),
                ("**kwargs", "Extra keyword arguments."),
            ],
        );
    }

    #[test]
    fn ignores_parameter_section_nested_in_container_section() {
        assert_parameter_documentation(
            "\
Example:
    Args:
        nested: Not parameter documentation.
Args:
    value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn ignores_parameter_section_in_rest_directive() {
        assert_parameter_documentation(
            "\
Summary.

.. note::
    Args:
        nested: Not parameter documentation.",
            &[],
        );
    }

    #[test]
    fn ignores_parameter_section_after_blank_line_in_rest_directive() {
        assert_parameter_documentation(
            "\
Summary.

.. note::

        Keyword Args:
            nested: Not parameter documentation.",
            &[],
        );
    }

    #[test]
    fn ignores_parameter_section_in_unordered_markdown_list_item() {
        assert_parameter_documentation(
            "\
Summary.

- Example:
    Args:
        nested: Not parameter documentation.",
            &[],
        );
    }

    #[test]
    fn ignores_parameter_section_after_blank_line_in_markdown_list_item() {
        assert_parameter_documentation(
            "\
Summary.

- Example:

        Args:
            nested: Not parameter documentation.",
            &[],
        );
    }

    #[test]
    fn ignores_parameter_section_in_ordered_markdown_list_item() {
        assert_parameter_documentation(
            "\
Summary.

1. Example:
    Args:
        nested: Not parameter documentation.",
            &[],
        );
    }

    #[test]
    fn ignores_parameter_section_in_rest_field_list() {
        assert_parameter_documentation(
            "\
Summary.

:param value: Example input.
    Args:
        nested: Not parameter documentation.",
            &[],
        );
    }

    #[test]
    fn ignores_parameter_section_in_rest_literal_block() {
        assert_parameter_documentation(
            "\
Summary.

Example::

        Args:
            nested: Not parameter documentation.",
            &[],
        );
    }

    #[test]
    fn resumes_after_markdown_fence() {
        assert_parameter_documentation(
            "\
Summary.

    ```text
    Args:
        nested: Not parameter documentation.
    ```

    Args:
        value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn resumes_after_rest_literal_block() {
        assert_parameter_documentation(
            "\
Summary.

    Example::

        Args:
            nested: Not parameter documentation.

    Args:
        value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn resumes_after_rest_directive() {
        assert_parameter_documentation(
            "\
.. note::
    Args:
        nested: Not parameter documentation.
Args:
    value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn resumes_after_unindented_markdown_fence_following_rest_literal_marker() {
        assert_parameter_documentation(
            "\
Example::

```
sample
```

Args:
    value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn backticks_in_fence_info_do_not_hide_parameter_sections() {
        assert_parameter_documentation(
            "\
```PRNGKey`` is accepted.

Args:
    value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn ignores_doctest_content_and_resumes_after_it() {
        assert_parameter_documentation(
            "        >>> example()
        Args:
            nested: Not parameter documentation.

        Args:
            value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn returns_structured_section_kinds_in_source_order() {
        let raw = "\
Args:
    value: Documentation.
Keyword Args:
    option: Optional.
Other Parameters:
    other: Other.
Returns:
    bool: Result.";
        let kinds = sections(raw)
            .map(|section| section.kind)
            .collect::<Vec<_>>();

        assert_eq!(
            kinds,
            [
                SectionKind::Parameters,
                SectionKind::KeywordArguments,
                SectionKind::OtherParameters,
                SectionKind::Returns,
            ]
        );
    }

    #[test]
    fn returns_section_fragments_and_range() {
        let raw = "    Args:
        value: Documentation.
Methods:
    helper: Method documentation.";
        let sections = sections(raw)
            .map(|section| (section.kind, section.body.fragments, &raw[section.range]))
            .collect::<Vec<_>>();

        assert_eq!(
            sections,
            vec![(
                SectionKind::Parameters,
                vec![BodyFragment::Item(Item {
                    display_name: "value".to_string(),
                    ty: None,
                    description: "Documentation.".to_string(),
                })],
                "    Args:\n        value: Documentation.",
            )]
        );
    }

    #[test]
    fn ends_populated_return_section_at_aligned_prose() {
        let raw = "\
Returns:
    bool: Result.
Additional details.";
        let sections = sections(raw)
            .map(|section| (section.kind, &raw[section.range]))
            .collect::<Vec<_>>();

        assert_eq!(
            sections,
            vec![(
                SectionKind::Returns,
                "\
Returns:
    bool: Result.",
            )]
        );
    }

    fn display_parameters(raw: &str) -> String {
        let normalized_source = crate::docstring::documentation_trim(raw);
        parameter_documentation(&normalized_source)
            .into_iter()
            .map(|(name, documentation)| {
                let documentation = documentation
                    .lines()
                    .map(|line| match line {
                        "" => "  │".to_string(),
                        _ => format!("  │ {line}"),
                    })
                    .join("\n");
                format!("{name}:\n{documentation}")
            })
            .join("\n")
    }

    #[track_caller]
    fn assert_parameter_documentation(raw: &str, expected: &[(&str, &str)]) {
        let normalized_source = crate::docstring::documentation_trim(raw);
        let parameters = parameter_documentation(&normalized_source);
        assert_eq!(parameters.len(), expected.len(), "{raw}");

        for &(name, documentation) in expected {
            assert_eq!(
                parameters.get(name).map(String::as_str),
                Some(documentation),
                "{raw}"
            );
        }
    }
}
