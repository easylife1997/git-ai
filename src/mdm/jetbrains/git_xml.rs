//! Structure-preserving editing of JetBrains' `options/git.xml`.
//!
//! The file is parsed with `roxmltree` only to locate nodes by byte range. Every edit is
//! then spliced into the original text, so comments, unrelated components/options and the
//! user's formatting stay untouched. Invalid XML is reported as an error, never rewritten.

use crate::error::GitAiError;
use roxmltree::{Document, Node};
use std::ops::Range;

pub const COMPONENT_NAME: &str = "Git.Application.Settings";
pub const OPTION_NAME: &str = "myPathToGit";

pub fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn option_xml(value: &str) -> String {
    format!(
        "<option name=\"{OPTION_NAME}\" value=\"{}\" />",
        escape_attr(value)
    )
}

/// A complete git.xml for the case where the file does not exist yet.
pub fn build_git_xml(value: &str) -> String {
    format!(
        "<application>\n  <component name=\"{COMPONENT_NAME}\">\n    {}\n  </component>\n</application>\n",
        option_xml(value)
    )
}

fn invalid(message: &str) -> GitAiError {
    GitAiError::Generic(format!("Invalid git.xml: {message}"))
}

fn parse(src: &str) -> Result<Document<'_>, GitAiError> {
    let doc = Document::parse(src).map_err(|error| invalid(&error.to_string()))?;
    if doc.root_element().tag_name().name() != "application" {
        return Err(invalid("root element is not <application>"));
    }
    Ok(doc)
}

fn elements<'a, 'i>(parent: Node<'a, 'i>, tag: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    parent
        .children()
        .filter(move |n| n.is_element() && n.tag_name().name() == tag)
}

fn git_components<'a, 'i>(doc: &'a Document<'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    elements(doc.root_element(), "component")
        .filter(|n| n.attribute("name") == Some(COMPONENT_NAME))
}

fn find_option<'a, 'i>(doc: &'a Document<'i>) -> Option<Node<'a, 'i>> {
    git_components(doc)
        .flat_map(|component| elements(component, "option"))
        .find(|n| n.attribute("name") == Some(OPTION_NAME))
}

/// Current decoded `myPathToGit` value, or `None` when the option is absent.
pub fn read_option_value(src: &str) -> Result<Option<String>, GitAiError> {
    let doc = parse(src)?;
    let value = find_option(&doc)
        .and_then(|option| option.attribute("value"))
        .map(str::to_string);
    Ok(value)
}

fn splice(src: &str, range: Range<usize>, replacement: &str) -> String {
    let mut out = src.to_string();
    out.replace_range(range, replacement);
    out
}

fn is_self_closing(src: &str, range: &Range<usize>) -> bool {
    src[range.clone()].trim_end().ends_with("/>")
}

/// Byte offset of the closing tag (`</...>`) of a non-self-closing element.
fn closing_tag_start(src: &str, range: &Range<usize>) -> Result<usize, GitAiError> {
    src[range.clone()]
        .rfind("</")
        .map(|offset| range.start + offset)
        .ok_or_else(|| invalid("element has no closing tag"))
}

/// Set `myPathToGit` to `value`, creating the option/component/root content as needed.
pub fn set_option_value(src: &str, value: &str) -> Result<String, GitAiError> {
    let doc = parse(src)?;
    let option = option_xml(value);

    if let Some(node) = find_option(&doc) {
        return Ok(splice(src, node.range(), &option));
    }

    if let Some(component) = git_components(&doc).next() {
        let range = component.range();
        if is_self_closing(src, &range) {
            let full =
                format!("<component name=\"{COMPONENT_NAME}\">\n    {option}\n  </component>");
            return Ok(splice(src, range, &full));
        }
        let close = closing_tag_start(src, &range)?;
        return Ok(splice(src, close..close, &format!("  {option}\n  ")));
    }

    let range = doc.root_element().range();
    let component =
        format!("  <component name=\"{COMPONENT_NAME}\">\n    {option}\n  </component>\n");
    if is_self_closing(src, &range) {
        let full = format!("<application>\n{component}</application>");
        return Ok(splice(src, range, &full));
    }
    let close = closing_tag_start(src, &range)?;
    Ok(splice(src, close..close, &component))
}

pub enum Removal {
    /// The option was not present; nothing to do.
    NotPresent,
    /// Removing the option leaves nothing else in the file.
    FileEmpty,
    /// The file content with only our option (and an emptied component) removed.
    Updated(String),
}

fn is_blank(node: Node) -> bool {
    node.children()
        .all(|c| c.is_text() && c.text().is_none_or(|t| t.trim().is_empty()))
}

/// Remove a byte range, together with its whole line when nothing else shares the line.
fn remove_range(src: &str, range: Range<usize>) -> String {
    let bytes = src.as_bytes();
    let mut start = range.start;
    while start > 0 && matches!(bytes[start - 1], b' ' | b'\t') {
        start -= 1;
    }
    let at_line_start = start == 0 || bytes[start - 1] == b'\n';
    let mut end = range.end;
    while end < bytes.len() && matches!(bytes[end], b' ' | b'\t' | b'\r') {
        end += 1;
    }
    let at_line_end = end >= bytes.len() || bytes[end] == b'\n';
    let (from, to) = if at_line_start && at_line_end {
        (start, (end + 1).min(bytes.len()))
    } else {
        (range.start, range.end)
    };
    format!("{}{}", &src[..from], &src[to..])
}

/// Remove only our option; drop the component if that leaves it empty.
pub fn remove_option(src: &str) -> Result<Removal, GitAiError> {
    let range = {
        let doc = parse(src)?;
        match find_option(&doc) {
            Some(option) => option.range(),
            None => return Ok(Removal::NotPresent),
        }
    };
    let mut text = remove_range(src, range);

    let empty_component = {
        let doc = parse(&text)?;
        git_components(&doc)
            .find(|c| is_blank(*c))
            .map(|c| c.range())
    };
    if let Some(range) = empty_component {
        text = remove_range(&text, range);
    }

    let empty_file = is_blank(parse(&text)?.root_element());
    if empty_file {
        Ok(Removal::FileEmpty)
    } else {
        Ok(Removal::Updated(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "$USER_HOME$/.easylife-ai/bin/git";

    fn value_of(xml: &str) -> Option<String> {
        read_option_value(xml).unwrap()
    }

    #[test]
    fn set_replaces_value_in_multiline_file_and_keeps_other_options() {
        let src = "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"myPathToGit\" value=\"/usr/bin/git\" />\n    <option name=\"other\" value=\"1\" />\n  </component>\n</application>\n";
        let out = set_option_value(src, V).unwrap();
        assert_eq!(value_of(&out).as_deref(), Some(V));
        assert!(out.contains("<option name=\"other\" value=\"1\" />"));
    }

    #[test]
    fn set_handles_single_line_document() {
        let src = "<application><component name=\"Git.Application.Settings\"><option name=\"myPathToGit\" value=\"/usr/bin/git\"/><option name=\"other\" value=\"1\"/></component><component name=\"Else\"/></application>";
        let out = set_option_value(src, V).unwrap();
        assert_eq!(value_of(&out).as_deref(), Some(V));
        assert!(out.contains("<option name=\"other\" value=\"1\"/>"));
        assert!(out.contains("<component name=\"Else\"/>"));
    }

    #[test]
    fn set_handles_multiline_option_element() {
        let src = "<application>\n  <component name=\"Git.Application.Settings\">\n    <option\n      name=\"myPathToGit\"\n      value=\"/usr/bin/git\" />\n  </component>\n</application>\n";
        let out = set_option_value(src, V).unwrap();
        assert_eq!(value_of(&out).as_deref(), Some(V));
        assert!(!out.contains("/usr/bin/git"));
        assert!(Document::parse(&out).is_ok());
    }

    #[test]
    fn set_expands_self_closing_component() {
        let src =
            "<application>\n  <component name=\"Git.Application.Settings\" />\n</application>\n";
        let out = set_option_value(src, V).unwrap();
        assert_eq!(value_of(&out).as_deref(), Some(V));
    }

    #[test]
    fn set_adds_option_to_component_without_it() {
        let src = "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"other\" value=\"1\" />\n  </component>\n</application>\n";
        let out = set_option_value(src, V).unwrap();
        assert_eq!(value_of(&out).as_deref(), Some(V));
        assert!(out.contains("name=\"other\""));
    }

    #[test]
    fn set_adds_component_next_to_other_components() {
        let src = "<application>\n  <component name=\"Other\">\n    <option name=\"x\" value=\"1\" />\n  </component>\n</application>\n";
        let out = set_option_value(src, V).unwrap();
        assert_eq!(value_of(&out).as_deref(), Some(V));
        assert!(out.contains("<component name=\"Other\">"));
        assert!(out.contains("name=\"x\""));
    }

    #[test]
    fn set_handles_self_closing_application() {
        let out = set_option_value("<application/>", V).unwrap();
        assert_eq!(value_of(&out).as_deref(), Some(V));
    }

    #[test]
    fn set_escapes_attribute_values() {
        let out = set_option_value("<application/>", "C:/a&b/\"x\"/git.exe").unwrap();
        assert_eq!(value_of(&out).as_deref(), Some("C:/a&b/\"x\"/git.exe"));
    }

    #[test]
    fn set_errors_on_invalid_xml_instead_of_rewriting() {
        assert!(set_option_value("<application><component", V).is_err());
        assert!(set_option_value("<other/>", V).is_err());
    }

    #[test]
    fn remove_keeps_other_options_and_components() {
        let src = "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"myPathToGit\" value=\"x\" />\n    <option name=\"other\" value=\"1\" />\n  </component>\n</application>\n";
        match remove_option(src).unwrap() {
            Removal::Updated(out) => {
                assert_eq!(value_of(&out), None);
                assert!(out.contains("name=\"other\""));
            }
            _ => panic!("expected Updated"),
        }
    }

    #[test]
    fn remove_drops_emptied_component_but_keeps_siblings() {
        let src = "<application>\n  <component name=\"Git.Application.Settings\">\n    <option name=\"myPathToGit\" value=\"x\" />\n  </component>\n  <component name=\"Other\" />\n</application>\n";
        match remove_option(src).unwrap() {
            Removal::Updated(out) => {
                assert!(!out.contains("Git.Application.Settings"));
                assert!(out.contains("<component name=\"Other\" />"));
            }
            _ => panic!("expected Updated"),
        }
    }

    #[test]
    fn remove_reports_empty_file_when_only_our_option_exists() {
        let src = build_git_xml("x");
        assert!(matches!(remove_option(&src).unwrap(), Removal::FileEmpty));
    }

    #[test]
    fn remove_handles_single_line_document() {
        let src = "<application><component name=\"Git.Application.Settings\"><option name=\"myPathToGit\" value=\"x\"/><option name=\"other\" value=\"1\"/></component></application>";
        match remove_option(src).unwrap() {
            Removal::Updated(out) => {
                assert_eq!(value_of(&out), None);
                assert!(out.contains("name=\"other\""));
            }
            _ => panic!("expected Updated"),
        }
    }

    #[test]
    fn remove_not_present() {
        let src = "<application><component name=\"Other\"/></application>";
        assert!(matches!(remove_option(src).unwrap(), Removal::NotPresent));
    }

    #[test]
    fn remove_errors_on_invalid_xml() {
        assert!(remove_option("<application><oops>").is_err());
    }
}
