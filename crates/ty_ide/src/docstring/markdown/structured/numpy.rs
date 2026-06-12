use crate::docstring::document::numpy;

use super::{Section, SectionItem, SectionKind};

/// Returns NumPy-style sections that can be rendered structurally.
pub(super) fn structured_sections(raw_source: &str, normalized_source: &str) -> Vec<Section> {
    numpy::sections(raw_source, normalized_source)
        .into_iter()
        .filter_map(section)
        .collect()
}

fn section(parsed: numpy::Section) -> Option<Section> {
    let (kind, range, fragments) = parsed.into_renderable_parts()?;
    let items = fragments
        .into_iter()
        .map(|fragment| section_item(kind, fragment))
        .collect::<Vec<_>>();
    if items.is_empty() {
        return None;
    }
    Section::new(range, items)
}

fn section_item(kind: SectionKind, fragment: numpy::BodyFragment) -> SectionItem {
    match fragment {
        numpy::BodyFragment::Prose(description) => {
            SectionItem::from_owned_parts(kind, None, None, description)
        }
        numpy::BodyFragment::Item(item) => {
            let (display_name, ty, description) = item.into_parts();
            SectionItem::from_owned_parts(kind, display_name, ty, description)
        }
    }
}

#[cfg(test)]
mod tests {
    use insta::{Settings, assert_snapshot};

    use super::super::render_sections_into;
    use super::structured_sections;

    #[test]
    fn renders_supported_sections() {
        let _snap = bind_markdown_snapshot_filters();
        let docstring = "\
Summary.

Parameters
----------
value, alias : str
    The value.
other
    Another value.

Other Parameters
----------------
kw_only: bool
    Less common option.

Attributes
----------
name : str
    Display name.

Returns
-------
result : bool
    Whether validation passed.

Yields
------
int
    Next value.

Raises
------
ValueError
    If invalid.
";

        assert_snapshot!(render_numpy(docstring), @r"
        Summary.

        ## Parameters
        **value, alias**: `str`<HB>
        The value.

        **other**<HB>
        Another value.

        ## Other Parameters
        **kw\_only**: `bool`<HB>
        Less common option.

        ## Attributes
        **name**: `str`<HB>
        Display name.

        ## Returns
        **result**: `bool`<HB>
        Whether validation passed.

        ## Yields
        `int`<HB>
        Next value.

        ## Raises
        `ValueError`<HB>
        If invalid.
        ");
    }

    #[test]
    fn renders_preformatted_parameter_description() {
        let _snap = bind_markdown_snapshot_filters();
        let docstring = "\
Parameters
----------
value : str
    Example::
        ```
other : int
    Another value.
";

        assert_snapshot!(render_numpy(docstring), @"
        ## Parameters
        **value**: `str`<HB>
        Example:

        ```````````python
            ```
        ```````````

        **other**: `int`<HB>
        Another value.
        ");
    }

    #[test]
    fn renders_parameter_section_preamble() {
        let _snap = bind_markdown_snapshot_filters();
        let docstring = "\
Parameters
----------
Either x or y must be provided.

beta : float
    Useful documentation.
";

        assert_snapshot!(render_numpy(docstring), @"
        ## Parameters
        Either x or y must be provided.

        **beta**: `float`<HB>
        Useful documentation.
        ");
    }

    #[test]
    fn renders_shifted_top_level_sections() {
        let _snap = bind_markdown_snapshot_filters();
        let docstring = "\
A decoded newline follows:
This line starts at column zero.

    Parameters
    ----------
    shifted : int
        Documentation in a shifted section.
";

        assert_snapshot!(render_numpy(docstring), @"
        A decoded newline follows:<HB>
        This line starts at column zero.

        ## Parameters
        **shifted**: `int`<HB>
        Documentation in a shifted section.
        ");
    }

    #[test]
    fn renders_parenthesized_return_names() {
        let _snap = bind_markdown_snapshot_filters();
        let docstring = "\
Returns
-------
((node1, node2), ancestor) : tuple[tuple[object, object], object]
    A node pair and its lowest common ancestor.
";

        assert_snapshot!(render_numpy(docstring), @"
        ## Returns
        **((node1, node2), ancestor)**: `tuple[tuple[object, object], object]`<HB>
        A node pair and its lowest common ancestor.
        ");
    }

    #[test]
    fn renders_return_prose_outside_the_structured_section() {
        let _snap = bind_markdown_snapshot_filters();
        let docstring = "\
Returns
-------
list of nodes
    The nodes in traversal order
necessarily returned in a stable order
";

        assert_snapshot!(render_numpy(docstring), @"
        ## Returns
        `list of nodes`<HB>
        The nodes in traversal order

        necessarily returned in a stable order
        ");
    }

    #[test]
    fn declines_to_render_nested_parameter_items() {
        let docstring = "\
Parameters
----------
Choose one of the following.
    nested : int
        Example-only text.
beta : float
    Useful documentation.
";

        let rendered = render_numpy(docstring);
        assert!(rendered.contains("**beta**: `float`"));
        assert!(!rendered.contains("**nested**"));
    }

    #[test]
    fn declines_to_render_section_nested_in_container() {
        let docstring = "\
Summary.

- Example data:
    Parameters
    ----------
    nested : int
        Not parameter documentation.
";

        assert!(parsed_sections(docstring).is_empty());
    }

    #[test]
    fn declines_to_render_prose_only_return_section() {
        let docstring = "\
Returns
-------
    The created object.
";

        assert!(parsed_sections(docstring).is_empty());
    }

    #[test]
    fn declines_to_render_empty_return_section() {
        let docstring = "\
Returns
-------

Notes
-----
Not a return value.
";

        assert!(parsed_sections(docstring).is_empty());
    }

    #[test]
    fn declines_to_render_indented_section() {
        let docstring = "\
Summary.

    Parameters
    ----------
    other : str
        Another value.
";

        assert!(parsed_sections(docstring).is_empty());
    }

    fn render_numpy(source: &str) -> String {
        let mut output = String::new();
        render_sections_into(&mut output, source, parsed_sections(source));
        output
    }

    fn parsed_sections(source: &str) -> Vec<super::Section> {
        structured_sections(source, source)
    }

    fn bind_markdown_snapshot_filters() -> impl Drop {
        let mut settings = Settings::clone_current();
        settings.add_filter("  \n", "<HB>\n");
        settings.bind_to_scope()
    }
}
