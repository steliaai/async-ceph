// Copyright (c) 2026 Stelia Ltd
// This project is dual-licensed under Apache 2.0 and MIT terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

use bindgen::callbacks::ParseCallbacks;
use std::{borrow::Cow, fmt::Write};

/// Very primitive markdown formatter for doxygen comments.
#[derive(Debug)]
pub struct DoxygenFormatter;

/// Escape markdown links and emphasis markers (`*`), unless the latter is around a single word
fn escape_markdown(comment: &str) -> String {
    let mut escaped = String::with_capacity(comment.len());
    let mut remaining = comment;

    while let Some(i_delim) = remaining.find(['[', ']', '*']) {
        let delim = remaining.as_bytes()[i_delim];
        let next = &remaining[i_delim + 1 ..];

        // Keep emphasis markers if they surround a word
        if delim == b'*'
            && let Some(i_space_or_star) = next.find([' ', '\n', '*'])
            && next.as_bytes()[i_space_or_star] == b'*'
        {
            let (prev, next) = remaining.split_at(i_delim + i_space_or_star + 2);
            escaped.push_str(prev);
            remaining = next;
        } else {
            write!(&mut escaped, "{}\\{}", &remaining[.. i_delim], delim as char).unwrap();
            remaining = next;
        }
    }
    escaped.push_str(remaining);
    escaped
}

impl ParseCallbacks for DoxygenFormatter {
    fn process_comment(&self, comment: &str) -> Option<String> {
        let escaped = escape_markdown(comment);
        let mut formatted = String::with_capacity(escaped.len());
        let mut in_group_desc = false;

        // doxygen keywords and the markdown section to map them to
        const SECTIONS: &[(&str, &str)] = &[
            ("pre", "Pre-call Requirements"),
            ("post", "On Success"),
            ("note", "Notes"),
            ("warning", "Warnings"),
            ("param", "Parameters"),
            ("returns", "Returns"),
        ];
        let mut section_entries = vec![vec![]; SECTIONS.len()];
        let mut section_order = Vec::new();
        let mut current_section = None;

        for line in escaped.lines().map(|l| l.trim()) {
            // ignore group markers, just use them to know when we exited the defgroup description
            if line == "@{" || line == "@}" {
                in_group_desc = false;
                continue;
            }
            // ignore any "module-level" group documentation, because we can't attach it to any item
            if in_group_desc {
                continue;
            }
            // if a group doc comment is right next to an item one, bindgen will merge them and leave
            // the doxygen ending and beginning markers in the comment. So ignore these types of lines
            if line == "/" || line == "/**" {
                continue;
            }

            if let Some((mut directive, args)) = line.strip_prefix("@").and_then(|p| p.split_once(' ')) {
                if directive == "defgroup" {
                    in_group_desc = true;
                    continue;
                }

                if directive == "return" {
                    directive = "returns"
                }

                if let Some(i) = SECTIONS.iter().position(|&s| s.0 == directive) {
                    current_section = Some(i);

                    let entries = &mut section_entries[i];
                    if entries.is_empty() {
                        section_order.push(i);
                    }
                    if directive == "param"
                        && let Some((name, desc)) = args.split_once(' ')
                    {
                        entries.push(Cow::Owned(format!("`{name}`: {desc}")))
                    } else {
                        entries.push(Cow::Borrowed(args))
                    }
                }
            } else if let Some(i) = current_section {
                if line.is_empty() {
                    current_section = None;
                    continue;
                }
                let current_section_str = section_entries[i].last_mut().unwrap().to_mut();
                write!(current_section_str, "\n{line}").unwrap();
            } else {
                writeln!(&mut formatted, "{line}").unwrap();
            }
        }

        for i in section_order {
            let title = SECTIONS[i].1;
            let entries = &section_entries[i];
            let dash = if entries.len() == 1 { "" } else { "- " };

            writeln!(&mut formatted, "\n# {title}").unwrap();
            for line in entries {
                writeln!(&mut formatted, "{dash}{line}\n").unwrap();
            }
        }

        Some(formatted)
    }
}
