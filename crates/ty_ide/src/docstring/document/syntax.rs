use ruff_python_trivia::leading_indentation;
use ruff_source_file::UniversalNewlines;
use ruff_text_size::{TextRange, TextSize};

use super::rst::is_field_list_marker;

/// Collects docstring lines without their universal-newline terminators while preserving their
/// source ranges.
///
/// For example, `first\r\nsecond` yields `first` at offset 0 and `second` at offset 7.
pub(super) fn parsed_lines(source: &str) -> Vec<ParsedLine<'_>> {
    source
        .universal_newlines()
        .map(|line| ParsedLine {
            text: line.as_str(),
            range: line.range(),
            indent: indentation(line.as_str()),
        })
        .collect()
}

/// A docstring line and its source range, excluding the newline terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::docstring) struct ParsedLine<'a> {
    /// The line text, excluding its newline terminator.
    pub(in crate::docstring) text: &'a str,
    /// The byte range of `text` within the source document.
    pub(super) range: TextRange,
    /// The indentation in the source document.
    pub(super) indent: TextSize,
}

/// Returns whether `line` starts with a `CommonMark` list-item marker.
///
/// `CommonMark` limits ordered-list markers to nine digits to avoid integer
/// overflow in browsers: <https://spec.commonmark.org/0.31.2/#list-items>.
pub(in crate::docstring) fn starts_with_markdown_list_item(line: &str) -> bool {
    let bytes = line.as_bytes();
    if matches!(bytes, [b'-' | b'+' | b'*', b' ' | b'\t', ..]) {
        return true;
    }

    let digits = bytes
        .iter()
        .take(9)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    digits > 0
        && matches!(bytes.get(digits), Some(b'.' | b')'))
        && matches!(bytes.get(digits + 1), Some(b' ' | b'\t'))
}

/// Returns whether `text` consists of a complete Markdown code span.
pub(in crate::docstring) fn is_markdown_code_span(text: &str) -> bool {
    let opening_backtick_run = text.bytes().take_while(|byte| *byte == b'`').count();
    let closing_backtick_run = text.bytes().rev().take_while(|byte| *byte == b'`').count();

    if opening_backtick_run == 0 // We didn't find any backticks

        // Backtick runs are of mismatched length
        || opening_backtick_run != closing_backtick_run

        // Backtick runs overlap
        || opening_backtick_run > text.len() / 2
    {
        return false;
    }

    let contents = &text[opening_backtick_run..text.len() - opening_backtick_run];

    // Within the matched outer backtick runs, there is non other backtick run
    // of the same length (i.e., a run that would close the span early).
    contents
        .split(|character| character != '`')
        .all(|run| run.len() != opening_backtick_run)
}

/// Returns whether `line` starts a block that owns its indented contents.
pub(super) fn starts_container_block(line: &str) -> bool {
    is_rest_directive_marker(line)
        || is_field_list_marker(line)
        || starts_with_markdown_list_item(line.trim_start())
}

fn is_rest_directive_marker(line: &str) -> bool {
    let Some(directive) = line.trim_start().strip_prefix(".. ") else {
        return false;
    };
    let Some((name, _)) = directive.split_once("::") else {
        return false;
    };

    !name.is_empty() && !name.chars().any(char::is_whitespace)
}

/// Splits at the first top-level colon, ignoring colons inside brackets, quoted strings, and
/// Markdown code spans.
///
/// If square or curly brackets are unclosed, falls back to the first colon outside parentheses.
/// This preserves item parsing for malformed type annotations.
pub(super) fn split_once_at_top_level_colon(line: &str) -> Option<(&str, &str)> {
    let mut nesting = BracketNesting::default();
    let mut fallback_colon = None;
    let mut index = 0;

    while index < line.len() {
        let character = line[index..].chars().next()?;
        match character {
            '\'' | '"' => {
                index = quoted_string_end(line, index, character);
                continue;
            }
            '`' => {
                index = code_span_end(line, index);
                continue;
            }
            ':' if nesting.is_top_level() => return Some(split_at_colon(line, index)),
            ':' if nesting.is_outside_parentheses() => {
                fallback_colon.get_or_insert(index);
            }
            _ => nesting.update(character),
        }
        index += character.len_utf8();
    }

    fallback_colon.map(|index| split_at_colon(line, index))
}

#[derive(Default)]
struct BracketNesting {
    parentheses: usize,
    square: usize,
    curly: usize,
}

impl BracketNesting {
    fn is_top_level(&self) -> bool {
        self.parentheses == 0 && self.square == 0 && self.curly == 0
    }

    fn is_outside_parentheses(&self) -> bool {
        self.parentheses == 0
    }

    /// Updates the nesting depth while tolerating unmatched closing brackets.
    fn update(&mut self, character: char) {
        match character {
            '(' => self.parentheses += 1,
            ')' => self.parentheses = self.parentheses.saturating_sub(1),
            '[' => self.square += 1,
            ']' => self.square = self.square.saturating_sub(1),
            '{' => self.curly += 1,
            '}' => self.curly = self.curly.saturating_sub(1),
            _ => {}
        }
    }
}

fn quoted_string_end(source: &str, start: usize, quote: char) -> usize {
    let content_start = start + quote.len_utf8();
    let mut escaped = false;
    for (offset, character) in source[content_start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
        } else if character == quote {
            return content_start + offset + character.len_utf8();
        }
    }
    source.len()
}

fn code_span_end(source: &str, start: usize) -> usize {
    let delimiter_len = source[start..]
        .bytes()
        .take_while(|byte| *byte == b'`')
        .count();
    let mut index = start + delimiter_len;
    while index < source.len() {
        if source.as_bytes()[index] == b'`' {
            let closing_len = source[index..]
                .bytes()
                .take_while(|byte| *byte == b'`')
                .count();
            index += closing_len;
            if closing_len == delimiter_len {
                return index;
            }
        } else {
            let Some(character) = source[index..].chars().next() else {
                return source.len();
            };
            index += character.len_utf8();
        }
    }
    source.len()
}

fn split_at_colon(line: &str, index: usize) -> (&str, &str) {
    (&line[..index], &line[index + ':'.len_utf8()..])
}

/// Splits the prefix and contents of a balanced trailing parenthesized group.
///
/// Parentheses inside quoted strings do not affect nesting.
pub(super) fn split_trailing_parenthesized_group(value: &str) -> Option<(&str, &str)> {
    if !value.ends_with(')') {
        return None;
    }

    let mut depth = 0usize;
    let mut outermost_opening = None;
    let mut index = 0;

    while index < value.len() {
        let character = value[index..].chars().next()?;
        match character {
            '\'' | '"' => {
                index = quoted_string_end(value, index, character);
                continue;
            }
            '(' => {
                if depth == 0 {
                    outermost_opening = Some(index);
                }
                depth += 1;
            }
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && index + character.len_utf8() == value.len() {
                    let opening = outermost_opening?;
                    return Some((&value[..opening], &value[opening + '('.len_utf8()..index]));
                }
            }
            _ => {}
        }
        index += character.len_utf8();
    }
    None
}

/// Calculates indentation width, advancing tabs to the next multiple of eight columns.
pub(super) fn indentation(line: &str) -> TextSize {
    TextSize::new(
        leading_indentation(line)
            .bytes()
            .fold(0u32, |column, byte| match byte {
                b'\t' => (column / 8 + 1) * 8,
                _ => column + 1,
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::{split_once_at_top_level_colon, split_trailing_parenthesized_group};

    #[test]
    fn splits_after_nested_brackets() {
        assert_eq!(
            split_once_at_top_level_colon("value (dict[str, list[{key: value}]]): Description"),
            Some(("value (dict[str, list[{key: value}]])", " Description"))
        );
    }

    #[test]
    fn ignores_colons_inside_quoted_strings() {
        assert_eq!(
            split_once_at_top_level_colon(r"value (Literal['a\'b:c']): Description"),
            Some((r"value (Literal['a\'b:c'])", " Description"))
        );
    }

    #[test]
    fn ignores_colons_inside_code_spans() {
        assert_eq!(
            split_once_at_top_level_colon("value (`a:b`): Description"),
            Some(("value (`a:b`)", " Description"))
        );
    }

    #[test]
    fn matches_code_span_delimiter_length() {
        assert_eq!(
            split_once_at_top_level_colon("value (``a`b:c``): Description"),
            Some(("value (``a`b:c``)", " Description"))
        );
    }

    #[test]
    fn recovers_from_unclosed_square_brackets() {
        assert_eq!(
            split_once_at_top_level_colon("value [str: Description"),
            Some(("value [str", " Description"))
        );
    }

    #[test]
    fn does_not_recover_from_unclosed_parentheses() {
        assert_eq!(
            split_once_at_top_level_colon("value (str: Description"),
            None
        );
    }

    #[test]
    fn splits_trailing_parenthesized_group() {
        assert_eq!(
            split_trailing_parenthesized_group("value (str)"),
            Some(("value ", "str"))
        );
    }

    #[test]
    fn splits_nested_parenthesized_group() {
        assert_eq!(
            split_trailing_parenthesized_group("value (Callable[(int), tuple[str]])"),
            Some(("value ", "Callable[(int), tuple[str]]"))
        );
    }

    #[test]
    fn ignores_parentheses_inside_quoted_strings() {
        assert_eq!(
            split_trailing_parenthesized_group("value (Literal[')'])"),
            Some(("value ", "Literal[')']"))
        );
    }

    #[test]
    fn ignores_parentheses_after_escaped_quotes() {
        assert_eq!(
            split_trailing_parenthesized_group(r#"value (Literal["a\"b)c"])"#),
            Some(("value ", r#"Literal["a\"b)c"]"#))
        );
    }

    #[test]
    fn rejects_unclosed_parenthesized_group() {
        assert_eq!(split_trailing_parenthesized_group("value (str"), None);
    }

    #[test]
    fn rejects_parenthesized_group_before_trailing_text() {
        assert_eq!(
            split_trailing_parenthesized_group("value (str) or None"),
            None
        );
    }
}
