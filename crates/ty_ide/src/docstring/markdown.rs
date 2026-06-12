mod general;
mod structured;

use super::{DocstringFragment, documentation_trim};

/// Renders Markdown for a source docstring.
pub(super) fn render(raw_source: &str) -> String {
    let normalized_source = documentation_trim(raw_source);
    let mut output = String::new();
    structured::render_into(&mut output, raw_source, &normalized_source);
    output
}

impl DocstringFragment {
    pub(super) fn render_markdown(&self) -> String {
        let mut output = String::new();
        general::render_fragment_into(&mut output, &self.0);
        output
    }
}
