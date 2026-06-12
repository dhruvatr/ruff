use std::borrow::Cow;

use indexmap::IndexMap;
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_source_file::UniversalNewlines;
use ruff_text_size::{TextRange, TextSize};

use super::SectionKind;
use super::preformatted::PreformattedBlockScanner;
use super::syntax::{
    ParsedLine, indentation, is_markdown_code_span, split_once_at_top_level_colon,
    split_trailing_parenthesized_group, starts_container_block,
};

/// Returns parameter documentation from recognized NumPy-style parameter sections.
pub(super) fn parameter_documentation(raw_source: &str) -> IndexMap<String, String> {
    let mut parameters = IndexMap::new();

    for section in parse_sections(raw_source, SectionIndentation::Structural) {
        let (kind, _, fragments) = section.into_parts();
        if matches!(kind, SectionKind::Parameters | SectionKind::OtherParameters) {
            extend_parameter_documentation(&mut parameters, fragments);
        }
    }

    parameters
}

/// Returns recognized NumPy-style sections in normalized source order.
pub(in crate::docstring) fn sections(raw_source: &str, normalized_source: &str) -> Vec<Section> {
    let line_offsets = NormalizedLineOffsets::new(raw_source, normalized_source);
    let raw_starts = parse_sections(raw_source, SectionIndentation::Structural)
        .filter_map(|section| line_offsets.normalized(section.range.start()))
        .collect::<Vec<_>>();

    parse_sections(normalized_source, SectionIndentation::Source)
        .filter(|section| raw_starts.binary_search(&section.range.start()).is_ok())
        .collect()
}

/// A recognized NumPy-style docstring section.
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

    /// Consumes this section when it can be rendered structurally.
    pub(in crate::docstring) fn into_renderable_parts(
        self,
    ) -> Option<(SectionKind, TextRange, Vec<BodyFragment>)> {
        self.body.is_renderable.then(|| self.into_parts())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SectionBody {
    fragments: Vec<BodyFragment>,
    is_renderable: bool,
}

impl SectionBody {
    fn opaque() -> Self {
        Self {
            fragments: Vec::new(),
            is_renderable: false,
        }
    }
}

/// One parsed fragment in a NumPy section body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::docstring) enum BodyFragment {
    /// Section-level prose that precedes the first named item.
    Prose(String),
    /// A named or anonymous section item.
    Item(Item),
}

/// A parsed item in a NumPy section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::docstring) struct Item {
    display_name: Option<String>,
    ty: Option<String>,
    description: String,
}

impl Item {
    /// Consumes this item and returns its display parts.
    pub(in crate::docstring) fn into_parts(self) -> (Option<String>, Option<String>, String) {
        (self.display_name, self.ty, self.description)
    }
}

/// Maps source-line starts to their offsets after PEP 257 normalization.
struct NormalizedLineOffsets {
    offsets: Vec<(TextSize, TextSize)>,
}

impl NormalizedLineOffsets {
    fn new(raw_source: &str, normalized_source: &str) -> Self {
        let raw_lines = super::syntax::parsed_lines(raw_source);
        let normalized_lines = super::syntax::parsed_lines(normalized_source);
        let first_content_line = raw_lines
            .iter()
            .position(|line| !line.text.trim().is_empty())
            .unwrap_or(raw_lines.len());
        let offsets = raw_lines[first_content_line..]
            .iter()
            .zip(normalized_lines)
            .map(|(raw, normalized)| (raw.range.start(), normalized.range.start()))
            .collect();
        Self { offsets }
    }

    fn normalized(&self, raw_offset: TextSize) -> Option<TextSize> {
        self.offsets
            .binary_search_by_key(&raw_offset, |&(raw_start, _)| raw_start)
            .ok()
            .map(|index| self.offsets[index].1)
    }
}

fn parsed_body_lines<'a>(body: &[NumpyLine<'a>]) -> Vec<ParsedLine<'a>> {
    body.iter()
        .map(|line| ParsedLine {
            text: line.text,
            range: line.range,
            indent: line.raw_indent,
        })
        .collect()
}

/// A NumPy docstring line with indentation before and after PEP 257 normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NumpyLine<'a> {
    text: &'a str,
    range: TextRange,
    raw_indent: TextSize,
    structural_indent: TextSize,
}

fn parsed_lines(raw: &str) -> Vec<NumpyLine<'_>> {
    let mut lines = raw
        .universal_newlines()
        .map(|line| NumpyLine {
            text: line.as_str(),
            range: line.range(),
            raw_indent: indentation(line.as_str()),
            structural_indent: TextSize::default(),
        })
        .collect::<Vec<_>>();

    let continuation_margin = lines
        .iter()
        .skip(1)
        .filter(|line| !line.text.trim().is_empty())
        .map(|line| line.raw_indent)
        .min()
        .unwrap_or_default();

    for line in lines.iter_mut().skip(1) {
        line.structural_indent = line.raw_indent.saturating_sub(continuation_margin);
    }

    lines
}

fn container_block_end(lines: &[NumpyLine<'_>], index: usize) -> Option<usize> {
    let marker = lines.get(index)?;
    if !starts_container_block(marker.text) {
        return None;
    }

    Some(
        (index + 1..lines.len())
            .find(|&end| {
                let line = lines[end];
                !line.text.trim().is_empty() && line.raw_indent <= marker.raw_indent
            })
            .unwrap_or(lines.len()),
    )
}

fn parse_sections(
    source: &str,
    section_indentation: SectionIndentation,
) -> impl Iterator<Item = Section> + '_ {
    let lines = parsed_lines(source);
    Parser::new(section_indentation).parse(&lines).into_iter()
}

struct Parser {
    section_indentation: SectionIndentation,
    sections: Vec<Section>,
}

impl Parser {
    fn new(section_indentation: SectionIndentation) -> Self {
        Self {
            section_indentation,
            sections: Vec::new(),
        }
    }

    fn parse(mut self, lines: &[NumpyLine<'_>]) -> Vec<Section> {
        let top_level_indent = effective_top_level_indent(lines, self.section_indentation);
        let mut preformatted_blocks = PreformattedBlockScanner::default();
        let mut index = 0;

        while index < lines.len() {
            if preformatted_blocks.consume_preformatted_line(lines[index].text) {
                index += 1;
                continue;
            }
            if let Some(container_end) = container_block_end(lines, index) {
                index = container_end;
                continue;
            }

            let Some(header) = parse_section_header(lines, index, self.section_indentation) else {
                if let Some(section_end) =
                    underlined_section_end(lines, index, self.section_indentation)
                {
                    index = section_end;
                    continue;
                }
                preformatted_blocks.observe_line_outside_preformatted_block(lines[index].text);
                index += 1;
                continue;
            };
            if header.indent(self.section_indentation) != top_level_indent {
                index += 1;
                continue;
            }

            let (body_end, range) = section_body_end(lines, header, self.section_indentation);
            self.sections.push(Section {
                kind: header.kind,
                range,
                body: parse_body(
                    header.kind,
                    &parsed_body_lines(&lines[header.body_start..body_end]),
                ),
            });
            index = body_end;
        }

        self.sections
    }
}

fn effective_top_level_indent(
    lines: &[NumpyLine<'_>],
    section_indentation: SectionIndentation,
) -> TextSize {
    // PEP 257 ignores the first line's indentation, so a lone column-zero first line cannot
    // distinguish a nested block from a shifted top-level section. A later column-zero logical
    // line can prevent physically top-level lines after an escaped newline from being dedented.
    if !lines
        .iter()
        .skip(1)
        .any(|line| !line.text.trim().is_empty() && indentation(line.text) == TextSize::default())
    {
        return TextSize::default();
    }

    let mut preformatted_blocks = PreformattedBlockScanner::default();
    let mut top_level_indent = None;
    let mut index = 0;

    while index < lines.len() {
        if preformatted_blocks.consume_preformatted_line(lines[index].text) {
            index += 1;
            continue;
        }
        if let Some(container_end) = container_block_end(lines, index) {
            index = container_end;
            continue;
        }

        if let Some(header) = parse_section_header(lines, index, section_indentation) {
            let header_indent = header.indent(section_indentation);
            top_level_indent = Some(
                top_level_indent
                    .map_or(header_indent, |indent: TextSize| indent.min(header_indent)),
            );
            index += 2;
            continue;
        }
        if let Some(section_end) = underlined_section_end(lines, index, section_indentation) {
            index = section_end;
            continue;
        }

        preformatted_blocks.observe_line_outside_preformatted_block(lines[index].text);
        index += 1;
    }

    top_level_indent.unwrap_or_default()
}

fn section_body_end(
    lines: &[NumpyLine<'_>],
    header: NumpySectionHeader,
    section_indentation: SectionIndentation,
) -> (usize, TextRange) {
    let mut body_end = header.body_start;
    let mut range = header.range;
    let mut preformatted_blocks = PreformattedBlockScanner::default();
    let first_item = first_body_item_index(lines, header, section_indentation);

    while let Some(line) = lines.get(body_end) {
        if preformatted_blocks.is_active()
            && preformatted_blocks.consume_preformatted_line(line.text)
        {
            range = TextRange::new(range.start(), line.range.end());
            body_end += 1;
            continue;
        }

        if first_item.is_some_and(|first_item| body_end < first_item) {
            if !preformatted_blocks.consume_preformatted_line(line.text) {
                preformatted_blocks.observe_line_outside_preformatted_block(line.text);
            }
            range = TextRange::new(range.start(), line.range.end());
            body_end += 1;
            continue;
        }

        if line.text.trim().is_empty() {
            if !blank_line_continues_section(&lines[body_end..], header, section_indentation) {
                break;
            }

            while let Some(blank_line) = lines.get(body_end)
                && blank_line.text.trim().is_empty()
            {
                range = TextRange::new(range.start(), blank_line.range.end());
                body_end += 1;
            }
            continue;
        }

        if underlined_section_indent(lines, body_end, section_indentation)
            .is_some_and(|indent| indent <= header.indent(section_indentation))
        {
            break;
        }

        if !line.text.trim().is_empty()
            && !line_belongs_to_body(header, line, &lines[body_end + 1..])
        {
            break;
        }

        if !preformatted_blocks.consume_preformatted_line(line.text) {
            preformatted_blocks.observe_line_outside_preformatted_block(line.text);
        }
        range = TextRange::new(range.start(), line.range.end());
        body_end += 1;
    }

    (body_end, range)
}

fn first_body_item_index(
    lines: &[NumpyLine<'_>],
    header: NumpySectionHeader,
    section_indentation: SectionIndentation,
) -> Option<usize> {
    if !matches!(
        header.kind,
        SectionKind::Parameters | SectionKind::OtherParameters
    ) {
        return Some(header.body_start);
    }

    let mut preformatted_blocks = PreformattedBlockScanner::default();
    let mut index = header.body_start;
    while let Some(line) = lines.get(index) {
        if preformatted_blocks.consume_preformatted_line(line.text) {
            index += 1;
            continue;
        }

        if underlined_section_indent(lines, index, section_indentation)
            .is_some_and(|indent| indent <= header.indent(section_indentation))
        {
            return None;
        }

        if !line.text.trim().is_empty() {
            let line_indent = indentation(line.text);
            if line_indent < header.raw_indent {
                return None;
            }
            if line_indent == header.raw_indent {
                if parameter_item_starts(line, &lines[index + 1..]) {
                    return Some(index);
                }
            }
        }

        preformatted_blocks.observe_line_outside_preformatted_block(line.text);
        index += 1;
    }

    None
}

fn blank_line_continues_section(
    lines: &[NumpyLine<'_>],
    header: NumpySectionHeader,
    section_indentation: SectionIndentation,
) -> bool {
    let Some((offset, non_blank_line)) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| !line.text.trim().is_empty())
    else {
        return false;
    };

    if underlined_section_indent(lines, offset, section_indentation)
        .is_some_and(|indent| indent <= header.indent(section_indentation))
    {
        return false;
    }

    line_belongs_to_body(header, non_blank_line, &lines[offset + 1..])
}

fn line_belongs_to_body(
    header: NumpySectionHeader,
    line: &NumpyLine<'_>,
    following_lines: &[NumpyLine<'_>],
) -> bool {
    let line_indent = indentation(line.text);
    if line_indent > header.raw_indent {
        return true;
    }
    if line_indent != header.raw_indent {
        return false;
    }
    match header.kind {
        SectionKind::Parameters | SectionKind::KeywordArguments | SectionKind::OtherParameters => {
            parameter_item_starts(line, following_lines)
        }
        SectionKind::Attributes => named_item_starts(line, following_lines),
        SectionKind::Returns | SectionKind::Yields => return_item_starts(line, following_lines),
        SectionKind::Raises => raise_item_starts(line),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NumpySectionHeader {
    kind: SectionKind,
    raw_indent: TextSize,
    structural_indent: TextSize,
    body_start: usize,
    range: TextRange,
}

impl NumpySectionHeader {
    fn indent(self, section_indentation: SectionIndentation) -> TextSize {
        match section_indentation {
            SectionIndentation::Source => self.raw_indent,
            SectionIndentation::Structural => self.structural_indent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionIndentation {
    Source,
    Structural,
}

fn parse_section_header(
    lines: &[NumpyLine<'_>],
    index: usize,
    section_indentation: SectionIndentation,
) -> Option<NumpySectionHeader> {
    let line = lines.get(index)?;
    let underline = lines.get(index + 1)?;
    underlined_section_indent(lines, index, section_indentation)?;

    Some(NumpySectionHeader {
        kind: section_kind(line.text)?,
        raw_indent: line.raw_indent,
        structural_indent: line.structural_indent,
        body_start: index + 2,
        range: TextRange::new(line.range.start(), underline.range.end()),
    })
}

fn underlined_section_indent(
    lines: &[NumpyLine<'_>],
    index: usize,
    section_indentation: SectionIndentation,
) -> Option<TextSize> {
    let line = lines.get(index)?;
    let underline = lines.get(index + 1)?;
    let line_indent = match section_indentation {
        SectionIndentation::Source => line.raw_indent,
        SectionIndentation::Structural => line.structural_indent,
    };
    let underline_indent = match section_indentation {
        SectionIndentation::Source => underline.raw_indent,
        SectionIndentation::Structural => underline.structural_indent,
    };

    (!line.text.trim().is_empty()
        && underline_indent == line_indent
        && is_underline(underline.text))
    .then_some(line_indent)
}

fn underlined_section_end(
    lines: &[NumpyLine<'_>],
    index: usize,
    section_indentation: SectionIndentation,
) -> Option<usize> {
    let header_indent = underlined_section_indent(lines, index, section_indentation)?;
    let mut section_end = index + 2;
    let mut preformatted_blocks = PreformattedBlockScanner::default();

    while section_end < lines.len() {
        if preformatted_blocks.consume_preformatted_line(lines[section_end].text) {
            section_end += 1;
            continue;
        }
        if underlined_section_indent(lines, section_end, section_indentation)
            .is_some_and(|indent| indent <= header_indent)
        {
            break;
        }
        preformatted_blocks.observe_line_outside_preformatted_block(lines[section_end].text);
        section_end += 1;
    }

    Some(section_end)
}

fn section_kind(line: &str) -> Option<SectionKind> {
    match line.trim().to_ascii_lowercase().as_str() {
        "parameters" => Some(SectionKind::Parameters),
        "other parameters" => Some(SectionKind::OtherParameters),
        "attributes" => Some(SectionKind::Attributes),
        "returns" => Some(SectionKind::Returns),
        "yields" => Some(SectionKind::Yields),
        "raises" => Some(SectionKind::Raises),
        _ => None,
    }
}

fn is_underline(line: &str) -> bool {
    let line = line.trim();
    line.len() >= 3 && line.chars().all(|char| char == '-')
}

fn parameter_item_starts(line: &NumpyLine<'_>, following_lines: &[NumpyLine<'_>]) -> bool {
    let trimmed = line.text.trim();
    if let Some(separator) = parse_type_separator(trimmed) {
        if !separator.requires_description_block {
            return true;
        }

        if !separator.ty.is_empty() {
            return true;
        }

        return following_lines
            .iter()
            .find(|line| !line.text.trim().is_empty())
            .is_some_and(|next| indentation(next.text) > indentation(line.text));
    }

    is_item_name(trimmed)
}

fn named_item_starts(line: &NumpyLine<'_>, following_lines: &[NumpyLine<'_>]) -> bool {
    let trimmed = line.text.trim();
    if let Some(separator) = parse_type_separator(trimmed) {
        return !separator.requires_description_block
            || has_indented_description(line, following_lines);
    }

    untyped_item_starts(trimmed, line, following_lines)
}

fn untyped_item_starts(
    trimmed: &str,
    line: &NumpyLine<'_>,
    following_lines: &[NumpyLine<'_>],
) -> bool {
    is_item_name(trimmed) && has_indented_description(line, following_lines)
}

fn return_item_starts(line: &NumpyLine<'_>, following_lines: &[NumpyLine<'_>]) -> bool {
    let trimmed = line.text.trim();
    if let Some(separator) = parse_return_type_separator(trimmed) {
        return !separator.requires_description_block
            || has_indented_description(line, following_lines);
    }

    is_anonymous_return_type(trimmed)
}

/// Returns whether `line` is a valid anonymous NumPy-style return type.
fn is_anonymous_return_type(line: &str) -> bool {
    !line.is_empty()
        && !line.ends_with('.')
        && !line.ends_with(':')
        && is_numpy_return_type_expression(line)
}

fn is_numpy_return_type_expression(ty: &str) -> bool {
    if is_markdown_code_span(ty) {
        return true;
    }

    if !has_numpy_return_type_characters(ty) {
        return false;
    }

    if !ty.chars().any(char::is_whitespace) {
        return true;
    }

    // Whitespace makes prose ambiguous, so require syntax that strongly indicates a type.
    is_subscript_style_numpy_return_type(ty)
        || ty.contains('|')
        || is_conventional_spaced_numpy_return_type(ty)
}

fn is_subscript_style_numpy_return_type(ty: &str) -> bool {
    ty.split_once('[')
        .is_some_and(|(name, _)| is_numpy_return_type_atom(name) && ty.ends_with(']'))
}

fn is_conventional_spaced_numpy_return_type(ty: &str) -> bool {
    let mut tokens = ty.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };
    if !is_numpy_return_type_atom(first) {
        return false;
    }

    let mut found_connector = false;
    while let Some(connector) = tokens.next() {
        if !matches!(connector, "of" | "or") {
            return false;
        }
        let Some(atom) = tokens.next() else {
            return false;
        };
        if !is_numpy_return_type_atom(atom) {
            return false;
        }
        found_connector = true;
    }

    found_connector
}

fn is_numpy_return_type_atom(atom: &str) -> bool {
    !atom.chars().any(char::is_whitespace) && has_numpy_return_type_characters(atom)
}

fn has_numpy_return_type_characters(expression: &str) -> bool {
    expression
        .chars()
        .next()
        .is_some_and(is_numpy_return_type_start)
        && expression.chars().all(is_numpy_return_type_char)
}

fn is_numpy_return_type_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || matches!(ch, '_' | '~' | ':' | '`' | '(')
}

fn is_numpy_return_type_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || "_.[](){},|\"':/ `~-".contains(ch)
}

fn raise_item_starts(line: &NumpyLine<'_>) -> bool {
    parse_raise_item(line.text.trim()).is_some()
}

fn parse_raise_item(line: &str) -> Option<&str> {
    let (name, description) = line
        .split_once(':')
        .map_or((line.trim(), None), |(name, description)| {
            (name.trim(), Some(description.trim()))
        });
    if !is_item_name(name) {
        return None;
    }

    Some(description.unwrap_or_default())
}

fn parse_body(kind: SectionKind, body: &[ParsedLine<'_>]) -> SectionBody {
    match kind {
        SectionKind::Parameters | SectionKind::KeywordArguments | SectionKind::OtherParameters => {
            parse_items(body, true, parse_named_item)
        }
        SectionKind::Attributes => parse_items(body, false, parse_named_item),
        SectionKind::Returns | SectionKind::Yields => parse_items(body, false, parse_return_item),
        SectionKind::Raises => parse_items(body, false, parse_raise_item_builder),
    }
}

fn parse_items<'a>(
    body: &[ParsedLine<'a>],
    preserve_leading_prose: bool,
    parse_item: impl Fn(&ParsedLine<'a>) -> Option<ItemBuilder<'a>>,
) -> SectionBody {
    let required_item_indent = if preserve_leading_prose {
        let Some(indent) = body
            .iter()
            .filter_map(|line| parse_item(line).map(|_| indentation(line.text)))
            .min()
        else {
            return SectionBody::opaque();
        };
        Some(indent)
    } else {
        None
    };
    let mut fragments = Vec::new();
    let mut current: Option<ItemBuilder<'a>> = None;
    let mut leading_prose = DescriptionBuilder::default();
    let mut item_indent = None;
    let mut is_renderable = true;

    for line in body {
        if line.text.trim().is_empty() {
            if let Some(current) = &mut current {
                current.description.push_continuation("");
            } else if preserve_leading_prose {
                leading_prose.push_continuation("");
            }
            continue;
        }

        let line_indent = indentation(line.text);
        if item_indent.map_or_else(
            || required_item_indent.is_none_or(|indent| line_indent == indent),
            |indent| line_indent == indent,
        ) {
            if let Some(item) = parse_item(line) {
                finish_description_fragment(&mut fragments, &mut leading_prose);
                finish_item_fragment(&mut fragments, &mut current);
                current = Some(item);
                item_indent.get_or_insert(line_indent);
                continue;
            }
            if item_indent.is_some() {
                is_renderable = false;
                break;
            }
        }
        if item_indent.is_some_and(|indent| line_indent < indent) {
            is_renderable = false;
            break;
        }

        if current.is_none() && preserve_leading_prose {
            leading_prose.push_line(line.text);
            continue;
        }

        let Some(current) = current.as_mut() else {
            is_renderable = false;
            break;
        };
        current.description.push_continuation(line.text);
    }

    finish_description_fragment(&mut fragments, &mut leading_prose);
    finish_item_fragment(&mut fragments, &mut current);
    SectionBody {
        is_renderable: is_renderable && !fragments.is_empty(),
        fragments,
    }
}

fn finish_description_fragment(
    fragments: &mut Vec<BodyFragment>,
    description: &mut DescriptionBuilder<'_>,
) {
    let description = std::mem::take(description).finish();
    if !description.is_empty() {
        fragments.push(BodyFragment::Prose(description));
    }
}

fn finish_item_fragment(fragments: &mut Vec<BodyFragment>, current: &mut Option<ItemBuilder<'_>>) {
    if let Some(item) = current.take() {
        fragments.push(BodyFragment::Item(item.finish()));
    }
}

fn parse_named_item<'a>(line: &ParsedLine<'a>) -> Option<ItemBuilder<'a>> {
    let trimmed = line.text.trim();
    let (name, ty) = if let Some(separator) = parse_type_separator(trimmed) {
        (separator.name, Some(separator.ty))
    } else {
        is_item_name(trimmed).then_some((trimmed, None))?
    };

    Some(ItemBuilder::new(Some(normalize_item_name(name)), ty, ""))
}

fn parse_return_item<'a>(line: &ParsedLine<'a>) -> Option<ItemBuilder<'a>> {
    let trimmed = line.text.trim();
    if let Some(separator) = parse_return_type_separator(trimmed) {
        return Some(ItemBuilder::new(
            Some(Cow::Borrowed(separator.name)),
            Some(separator.ty),
            "",
        ));
    }
    if split_once_at_top_level_colon(trimmed)
        .is_some_and(|(name, _)| name.chars().last().is_some_and(char::is_whitespace))
    {
        return None;
    }

    is_anonymous_return_type(trimmed).then(|| ItemBuilder::new(None, Some(trimmed), ""))
}

fn parse_raise_item_builder<'a>(line: &ParsedLine<'a>) -> Option<ItemBuilder<'a>> {
    let (name, description) = line
        .text
        .trim()
        .split_once(':')
        .map_or((line.text.trim(), ""), |(name, description)| {
            (name.trim(), description.trim())
        });
    is_item_name(name).then(|| ItemBuilder::new(Some(Cow::Borrowed(name)), None, description))
}

struct ItemBuilder<'a> {
    display_name: Option<Cow<'a, str>>,
    ty: Option<&'a str>,
    description: DescriptionBuilder<'a>,
}

impl<'a> ItemBuilder<'a> {
    fn new(
        display_name: Option<Cow<'a, str>>,
        ty: Option<&'a str>,
        inline_description: &'a str,
    ) -> Self {
        Self {
            display_name,
            ty,
            description: DescriptionBuilder::with_inline(inline_description),
        }
    }

    fn finish(self) -> Item {
        Item {
            display_name: self.display_name.map(Cow::into_owned),
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

fn extend_parameter_documentation(
    parameters: &mut IndexMap<String, String>,
    fragments: Vec<BodyFragment>,
) {
    for fragment in fragments {
        let BodyFragment::Item(item) = fragment else {
            continue;
        };
        let (display_name, _, description) = item.into_parts();
        let Some(display_name) = display_name else {
            continue;
        };
        let description = description.trim();
        if description.is_empty() {
            continue;
        }
        let Some(names) = parameter_lookup_names(&display_name) else {
            continue;
        };
        for name in names {
            parameters.insert(name, description.to_string());
        }
    }
}

fn parameter_lookup_names(display_name: &str) -> Option<Vec<String>> {
    let mut lookup_names = Vec::new();
    for name in display_name.split(',').map(str::trim) {
        if name == "..." {
            continue;
        }

        let name = normalize_item_name(name);
        if !is_item_name_part(&name) {
            return None;
        }
        lookup_names.push(name.into_owned());
    }

    (!lookup_names.is_empty()).then_some(lookup_names)
}

/// A parsed NumPy-style `name : type` separator.
struct TypeSeparator<'a> {
    /// The documented item name.
    name: &'a str,
    /// The documented item type.
    ty: &'a str,
    /// Whether the separator requires an indented description to disambiguate it from prose.
    requires_description_block: bool,
}

/// Parses a NumPy-style `name : type` separator.
fn parse_type_separator(line: &str) -> Option<TypeSeparator<'_>> {
    parse_type_separator_if(line, is_item_name)
}

fn parse_return_type_separator(line: &str) -> Option<TypeSeparator<'_>> {
    parse_type_separator_if(line, |name| {
        is_item_name(name) || is_parenthesized_return_name(name)
    })
}

fn parse_type_separator_if(
    line: &str,
    is_valid_name: impl FnOnce(&str) -> bool,
) -> Option<TypeSeparator<'_>> {
    let (name, ty) = split_once_at_top_level_colon(line)?;
    let has_whitespace_before_colon = name.chars().last().is_some_and(char::is_whitespace);
    let has_whitespace_after_colon = ty.chars().next().is_some_and(char::is_whitespace);
    if !has_whitespace_before_colon && !has_whitespace_after_colon && !ty.is_empty() {
        return None;
    }

    let name = name.trim();
    let ty = ty.trim();
    if !is_valid_name(name) {
        return None;
    }
    Some(TypeSeparator {
        name,
        ty,
        requires_description_block: !has_whitespace_before_colon,
    })
}

/// Returns whether `name` is a balanced tuple of valid NumPy item names.
fn is_parenthesized_return_name(name: &str) -> bool {
    let Some((prefix, elements)) = split_trailing_parenthesized_group(name) else {
        return false;
    };
    if !prefix.is_empty() {
        return false;
    }

    let mut depth = 0usize;
    let mut start = 0;
    let mut count = 0;
    for (index, character) in elements.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                let Some(nested_depth) = depth.checked_sub(1) else {
                    return false;
                };
                depth = nested_depth;
            }
            ',' if depth == 0 => {
                if !is_return_name_element(&elements[start..index]) {
                    return false;
                }
                count += 1;
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }

    depth == 0 && is_return_name_element(&elements[start..]) && count > 0
}

fn is_return_name_element(element: &str) -> bool {
    let element = element.trim();
    is_item_name_part(element) || is_parenthesized_return_name(element)
}

fn has_indented_description(line: &NumpyLine<'_>, following_lines: &[NumpyLine<'_>]) -> bool {
    following_lines
        .iter()
        .find(|line| !line.text.trim().is_empty())
        .is_some_and(|next| indentation(next.text) > indentation(line.text))
}

/// Returns whether `name` is a valid NumPy-style item name or comma-separated name list.
fn is_item_name(name: &str) -> bool {
    let mut has_lookup_name = false;
    let valid = name.split(',').all(|part| {
        let part = part.trim();
        if part == "..." {
            return true;
        }

        let part = normalize_item_name(part);
        if is_item_name_part(&part) {
            has_lookup_name = true;
            true
        } else {
            false
        }
    });

    valid && has_lookup_name
}

/// Removes reStructuredText escapes from NumPy variadic parameter names.
fn normalize_item_name(name: &str) -> Cow<'_, str> {
    if name.contains(r"\*") {
        Cow::Owned(name.replace(r"\*", "*"))
    } else {
        Cow::Borrowed(name)
    }
}

fn is_item_name_part(name: &str) -> bool {
    let name = name
        .strip_prefix("**")
        .or_else(|| name.strip_prefix('*'))
        .unwrap_or(name);

    !name.is_empty() && name.split('.').all(is_identifier)
}
