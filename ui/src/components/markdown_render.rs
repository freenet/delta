//! Markdown rendering for page content, for both the page view and the
//! editor's live preview.
//!
//! Page content is written by other people, so the work spent rendering it is
//! bounded: text past the limits below is shown as escaped plain text instead
//! of markdown. The `markdown` crate is pinned to a patched build in the root
//! Cargo.toml (`[patch.crates-io]`).
//!
//! Adapted from River's message rendering (`ui/src/components/conversation.rs`
//! in freenet/river), with limits sized for long documents rather than chat
//! messages.

use super::page_view::{finalize_anchors, inject_heading_ids};

/// Longest text rendered as markdown.
///
/// 32 times River's message limit: long enough for a long document (the
/// largest markdown file across the Freenet and mediator repositories is
/// under 60 KiB). Ordinary text this long renders in about 0.1 s; the
/// slowest text found that passes every limit here takes about 0.7 s
/// natively and 1 s in the browser. (The patched `markdown` build parses in
/// time linear in a document's length apart from the inline cost below;
/// the 1.0.0 release is quadratic, so this limit is only safe with it.)
const MARKDOWN_MAX_SOURCE_BYTES: usize = 128 * 1024;

/// Most block containers (`>`, `-`, `1.` ...) opened at the start of one line.
const MARKDOWN_MAX_LINE_NESTING: usize = 16;

/// Bound on the sum, over each run of non-blank lines, of the run's length
/// squared, in bytes squared.
///
/// Inline parsing (emphasis, brackets, code spans, inline HTML) can take time
/// quadratic in the length of a paragraph, and a paragraph never spans a
/// blank line. This allows a single 12 KiB run, or 36 runs of 2 KiB, and so
/// on. The largest real document measured (a 40 KiB design document with a
/// 9 KiB run) uses 70% of it, and 99% of the markdown files measured use
/// under a quarter.
const MARKDOWN_MAX_BLOCK_COST: u64 = 12 * 1024 * 12 * 1024;

/// Most table cells the HTML can contain. A table's body rows are padded
/// to the width of its delimiter row, so a wide delimiter row followed by
/// many short lines expands to columns times rows cells.
const MARKDOWN_MAX_TABLE_CELLS: usize = 64 * 1024;

/// Bound on `]:` occurrences times `]` occurrences. Each closing bracket is
/// checked against every definition, so many of both is quadratic.
const MARKDOWN_MAX_REFERENCE_LOOKUPS: usize = 1024 * 1024;

/// Longest HTML kept from rendering markdown. It also bounds the HTML that
/// references can expand to before rendering (see `references_are_bounded`).
const MARKDOWN_MAX_HTML_BYTES: usize = 4 * 1024 * 1024;

/// Render page markdown to HTML, with heading ids and finalized anchors.
///
/// `text` is what to render as markdown. `plain` is what to show, escaped, if
/// `text` is too costly to render: the content as the author wrote it, before
/// any Delta-specific rewriting.
pub(super) fn render_page_html(
    text: &str,
    plain: &str,
    rewrite_freenet_hrefs: bool,
    own_contract_id: Option<&str>,
) -> String {
    let Some(html) = render_gfm_bounded(text) else {
        return plain_text_to_html(plain);
    };
    let html = inject_heading_ids(&html);
    let html = finalize_anchors(&html, rewrite_freenet_hrefs, own_contract_id);
    if html.len() > MARKDOWN_MAX_HTML_BYTES {
        return plain_text_to_html(plain);
    }
    html
}

/// GFM HTML for `text`, or `None` if it is too costly to render.
fn render_gfm_bounded(text: &str) -> Option<String> {
    let block_cost = markdown_parse_cost(text)?;
    if !references_are_bounded(text, block_cost) {
        return None;
    }
    // GFM without MDX never fails to parse.
    let html = markdown::to_html_with_options(text, &markdown::Options::gfm()).ok()?;
    (html.len() <= MARKDOWN_MAX_HTML_BYTES).then_some(html)
}

/// Whether `text` is cheap enough to parse as markdown. See the limits above.
#[cfg(test)]
fn markdown_cost_is_bounded(text: &str) -> bool {
    markdown_parse_cost(text).is_some()
}

/// `text`'s block cost (see `MARKDOWN_MAX_BLOCK_COST`) if it is cheap enough
/// to parse as markdown, or `None`.
fn markdown_parse_cost(text: &str) -> Option<u64> {
    if text.len() > MARKDOWN_MAX_SOURCE_BYTES {
        return None;
    }
    let closing_brackets = text.matches(']').count();
    let definitions = text.matches("]:").count();
    if closing_brackets.saturating_mul(definitions) > MARKDOWN_MAX_REFERENCE_LOOKUPS {
        return None;
    }
    // A byte order mark can hide a marker from the counts below but not from
    // the parser, which skips a leading one.
    let text: std::borrow::Cow<str> = if text.contains('\u{feff}') {
        text.replace('\u{feff}', "").into()
    } else {
        text.into()
    };

    let mut block_cost: u64 = 0;
    let mut run_bytes: u64 = 0;
    let mut table_cells: usize = 0;
    let mut run_table_columns: usize = 0;
    for line in markdown_lines(&text) {
        if line_container_depth(line) > MARKDOWN_MAX_LINE_NESTING {
            return None;
        }
        if is_blank_line(line) {
            block_cost = block_cost.saturating_add(run_bytes * run_bytes);
            run_bytes = 0;
            run_table_columns = 0;
            continue;
        }
        run_bytes += line.len() as u64 + 1;
        if let Some(columns) = delimiter_row_columns(line) {
            run_table_columns = run_table_columns.max(columns);
        }
        // Every later line of the run may be a row of a table this wide.
        table_cells = table_cells.saturating_add(run_table_columns);
    }
    block_cost = block_cost.saturating_add(run_bytes * run_bytes);
    (block_cost <= MARKDOWN_MAX_BLOCK_COST && table_cells <= MARKDOWN_MAX_TABLE_CELLS)
        .then_some(block_cost)
}

/// The lines of `text`, split at `\n`, `\r\n` or a lone `\r` as markdown does.
fn markdown_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(text);
    std::iter::from_fn(move || {
        let current = rest?;
        match current.find(['\n', '\r']) {
            Some(end) => {
                let after = if current[end..].starts_with("\r\n") {
                    end + 2
                } else {
                    end + 1
                };
                rest = Some(&current[after..]);
                Some(&current[..end])
            }
            None => {
                rest = None;
                Some(current)
            }
        }
    })
}

/// A line of only spaces and tabs: the only kind of line that ends every
/// paragraph and table. (Other whitespace, such as a no-break space, does not
/// make a line blank.)
fn is_blank_line(line: &str) -> bool {
    line.bytes().all(|b| b == b' ' || b == b'\t')
}

/// If `line` could be a table's delimiter row (`| --- | :-: |`, possibly
/// inside block quotes), how many columns it gives the table: the cells
/// between pipes that contain a `-`. Every delimiter row is made of only
/// these characters and has a `-` in every cell, so a delimiter row is never
/// missed and its columns are never under-counted.
fn delimiter_row_columns(line: &str) -> Option<usize> {
    if !line
        .bytes()
        .all(|b| matches!(b, b'|' | b'-' | b':' | b' ' | b'\t' | b'>'))
    {
        return None;
    }
    let columns = line.split('|').filter(|cell| cell.contains('-')).count();
    (columns > 0).then_some(columns)
}

/// How many block quote, list item or footnote definition markers open at
/// the start of `line` (an over-count is fine: it only makes plain text more
/// likely).
fn line_container_depth(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut depth = 0;
    loop {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let marker_end = match bytes.get(i) {
            Some(b'>') => i + 1,
            // A GFM footnote definition, `[^label]: `.
            Some(b'[') if bytes.get(i + 1) == Some(&b'^') => match line[i..].find("]:") {
                Some(end) => {
                    depth += 1;
                    i += end + 2;
                    continue;
                }
                None => return depth,
            },
            Some(b'-' | b'*' | b'+') => i + 1,
            Some(b'0'..=b'9') => {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                match bytes.get(j) {
                    Some(b'.' | b')') => j + 1,
                    _ => return depth,
                }
            }
            _ => return depth,
        };
        // A list marker must be followed by whitespace or the line's end.
        let is_quote = bytes[i] == b'>';
        if !is_quote && !matches!(bytes.get(marker_end), None | Some(b' ' | b'\t')) {
            return depth;
        }
        depth += 1;
        i = marker_end;
    }
}

/// Whether the HTML that link and image references expand to is bounded.
///
/// Each reference copies its definition's destination and title into the
/// HTML, so a few long definitions used many times expand a page
/// quadratically. A cheap bound comes first: every reference ends in `]`, and
/// see `longest_possible_definition`. Only if that bound is too high is the
/// text parsed to count exactly, and since that parses it a second time, only
/// text within half of the size and block cost limits gets that far.
fn references_are_bounded(text: &str, block_cost: u64) -> bool {
    /// Bytes of HTML per byte of a copied destination or title (`"` becomes
    /// `&quot;`), and per reference for the tags around it.
    const PER_BYTE: usize = 6;
    const PER_REFERENCE: usize = 64;

    let closing_brackets = text.matches(']').count();
    let longest_definition = longest_possible_definition(text);
    let cheap_bound = closing_brackets.saturating_mul(
        longest_definition
            .saturating_mul(PER_BYTE)
            .saturating_add(PER_REFERENCE),
    );
    if cheap_bound <= MARKDOWN_MAX_HTML_BYTES {
        return true;
    }
    if text.len() > MARKDOWN_MAX_SOURCE_BYTES / 2 || block_cost > MARKDOWN_MAX_BLOCK_COST / 2 {
        return false;
    }

    let Ok(tree) = markdown::to_mdast(text, &markdown::ParseOptions::gfm()) else {
        return false;
    };
    let expansion = reference_expansion(&tree, longest_definition, PER_BYTE, PER_REFERENCE);
    drop_mdast(tree);
    expansion <= MARKDOWN_MAX_HTML_BYTES
}

/// An upper bound on the destination plus title of any one link reference
/// definition.
///
/// A definition is `[label]:`, a destination on that line or the next, and
/// an optional title opened by `"`, `'` or `(` on the destination's line or
/// the next. So a `]:` with none of those three characters in the rest of
/// its line and the two lines after it has nothing to copy beyond those
/// lines. Otherwise the title may run on, but never past a blank line, so
/// the bound is the rest of the run of non-blank lines.
fn longest_possible_definition(text: &str) -> usize {
    let lines: Vec<&str> = markdown_lines(text).collect();
    // Bytes from the start of each line to the end of its run, counting each
    // line ending as two bytes (`\r\n`) so it never under-counts.
    let mut run_tail = vec![0usize; lines.len()];
    let mut tail = 0;
    for (i, line) in lines.iter().enumerate().rev() {
        tail = if is_blank_line(line) {
            0
        } else {
            tail + line.len() + 2
        };
        run_tail[i] = tail;
    }
    let mut longest = 0;
    for (i, line) in lines.iter().enumerate() {
        for (pos, _) in line.match_indices("]:") {
            let rest = &line[pos + 2..];
            let next: Vec<&str> = lines.iter().skip(i + 1).take(2).copied().collect();
            let may_have_title = std::iter::once(rest)
                .chain(next.iter().copied())
                .any(|l| l.contains(['"', '\'', '(']));
            let bound = if may_have_title {
                run_tail[i] - pos
            } else {
                rest.len() + next.iter().map(|l| l.len() + 2).sum::<usize>()
            };
            longest = longest.max(bound);
        }
    }
    longest
}

/// Bytes of HTML the references in `tree` expand to (an over-estimate).
fn reference_expansion(
    root: &markdown::mdast::Node,
    fallback_definition: usize,
    per_byte: usize,
    per_reference: usize,
) -> usize {
    use markdown::mdast::Node;
    use std::collections::HashMap;

    let mut definitions: HashMap<&str, usize> = HashMap::new();
    let mut references: Vec<&str> = Vec::new();
    // Iterative, not recursive: nesting depth is chosen by whoever wrote the
    // text, and a recursive walk of a deep tree overflows the stack. Pre-order,
    // so definitions are seen in document order and the first one wins, as it
    // does when rendering.
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node {
            Node::Definition(d) => {
                let len = d.url.len() + d.title.as_ref().map_or(0, String::len);
                definitions.entry(d.identifier.as_str()).or_insert(len);
            }
            Node::LinkReference(r) => references.push(r.identifier.as_str()),
            Node::ImageReference(r) => references.push(r.identifier.as_str()),
            _ => {}
        }
        if let Some(children) = node.children() {
            stack.extend(children.iter().rev());
        }
    }
    references.iter().fold(0usize, |total, id| {
        // Every reference in the tree resolved to a definition; if the two
        // disagree on an identifier, assume the longest possible one.
        let len = definitions.get(id).copied().unwrap_or(fallback_definition);
        total.saturating_add(len.saturating_mul(per_byte).saturating_add(per_reference))
    })
}

/// Drop a markdown AST without recursing: the tree's own `Drop` recurses once
/// per nesting level, which overflows the stack on deeply nested text.
fn drop_mdast(root: markdown::mdast::Node) {
    let mut stack = vec![root];
    while let Some(mut node) = stack.pop() {
        if let Some(children) = node.children_mut() {
            stack.append(children);
        }
    }
}

/// Text not rendered as markdown: escaped, with each line break kept.
fn plain_text_to_html(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    format!("<p>{}</p>", escape_html(&text).replace('\n', "<br />\n"))
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN_ID: &str = "EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr";

    fn references_bounded(text: &str) -> bool {
        references_are_bounded(text, markdown_parse_cost(text).expect("cheap to parse"))
    }

    /// Both rendering paths (page view and editor preview share
    /// `render_page_html`), with and without the gateway href rewrite.
    fn render_every_way(text: &str) {
        for (rewrite, own) in [(true, None), (false, Some(OWN_ID)), (true, Some(OWN_ID))] {
            let _ = render_page_html(text, text, rewrite, own);
        }
    }

    /// Inputs the unpatched `markdown` crate panics on (see
    /// `[patch.crates-io]` in the root Cargo.toml).
    #[test]
    fn malformed_markdown_renders_without_panicking() {
        let inputs = [
            // A line ending inside a link title or reference label.
            "[a](b \"x\ny\")",
            "[a](b 'x\ny')",
            "[a](b (x\ny))",
            "![a](b \"x\ny\")",
            "[a](b \"x \ny\")",
            "[a](b \"x \r\ny\")",
            "[x](/x \"> \n\")",
            "[a][b\nc]\n\n[b c]: d",
            "[][a \n]\n\n[a ]:\0",
            // An email address in an image title that spans lines.
            "![a](b \"c@d.com\ne\")",
            // Setext underlines next to each other.
            "=\n=\n=\na\n=",
            "}\n-\n--\n]\n=",
            // A list item ending in unclosed code or HTML, then another marker.
            "1. <!--\n-",
            "*\t~~~\n1.",
            "- ```\n1)",
            // An unfinished CDATA opener, then an empty numeric reference.
            "<![C&#;",
            // A table head, then a new container on the last line.
            "a\n|-\n- <",
            "a\n|-\n> <",
        ];
        for input in inputs {
            for text in [
                input.to_string(),
                format!("# Title\n\n{input}"),
                format!("[[Some page]] {input}"),
            ] {
                let rendered = std::panic::catch_unwind(|| render_every_way(&text));
                assert!(rendered.is_ok(), "rendering {text:?} panicked");
            }
        }
    }

    /// A seeded sweep over short strings of markdown syntax, the shape that
    /// found every input above. It is deterministic (fixed seed), so a failure
    /// reproduces exactly; the panic message names the input.
    #[test]
    fn generated_markdown_renders_without_panicking() {
        #[rustfmt::skip]
        const PIECES: &[&str] = &[
            "[", "]", "(", ")", "\"", "'", " ", "  ", "\t", "\n", "\n", "\r\n", "\r", "a",
            "x y", "!", ":", "<", ">", "*", "_", "~", "`", "```", "~~~", "\\", "-", "#", "|",
            "^", "=", "&", "&amp;", "&#;", "&#65;", "https://x.example", "www.a.example",
            "c@d.example", "1.", "1)", "é", "\u{a0}", "😀", "[a](b \"", "[a](b '", "[a](b (",
            "[a][", "[^", "]: ", "![", "](", "<a ", "<!--", "-->", "<![C", "    ", "> ",
            "- ", "---", "| - |", "\\\n", "[ ]", "[x]", "\0", "[[", "]]", "\n\n",
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3_000 {
            let len = 1 + (next() % 24) as usize;
            let text: String = (0..len)
                .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
                .collect();
            let rendered = std::panic::catch_unwind(|| render_every_way(&text));
            assert!(rendered.is_ok(), "rendering {text:?} panicked");
        }
    }

    /// The AST helpers walk and drop a tree without recursion, so a deep tree
    /// cannot overflow the stack. Runs on a small stack to leave a margin
    /// below wasm's 1 MiB.
    #[test]
    fn deep_markdown_trees_are_walked_without_recursion() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let deep = format!("{}[a]\n\n[a]: b", ">".repeat(5_000));
                let tree = markdown::to_mdast(&deep, &markdown::ParseOptions::gfm()).unwrap();
                assert_eq!(reference_expansion(&tree, 0, 6, 64), 6 + 64);
                drop_mdast(tree);
            })
            .expect("spawn")
            .join()
            .expect("walking a deep markdown tree panicked");
    }

    /// The deepest nesting that the exact reference count accepts renders, on
    /// a small stack. Nesting across lines needs growing indentation, which
    /// the size limit bounds.
    #[test]
    fn deepest_allowed_nesting_renders_on_a_small_stack() {
        let mut text = String::new();
        let mut depth = 0;
        loop {
            let line = format!("{}- [a]\n\n", "  ".repeat(depth));
            // Half the size limit, the most the exact reference count allows.
            if text.len() + line.len() + 6_100 > MARKDOWN_MAX_SOURCE_BYTES / 2 {
                break;
            }
            text.push_str(&line);
            depth += 1;
        }
        // A definition followed, in the same run, by a long paragraph, so the
        // cheap reference bound fails and the tree is built and walked.
        text.push_str(&format!("[a]: b\n{}\n", "word ".repeat(1_200)));
        assert!(depth > 200, "{depth}");
        assert!(markdown_cost_is_bounded(&text));
        let cheap = text.matches(']').count() * (longest_possible_definition(&text) * 6 + 64);
        assert!(cheap > MARKDOWN_MAX_HTML_BYTES, "{cheap}");
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let html = render_page_html(&text, &text, true, None);
                assert!(html.contains("<ul>"), "{}", &html[..200]);
            })
            .expect("spawn")
            .join()
            .expect("rendering deep nesting panicked");
    }

    /// Deep block nesting on one line, a wide table, too much text, or too
    /// much text without a blank line is shown as plain text, however the
    /// line breaks are written.
    #[test]
    fn costly_markdown_is_shown_as_plain_text() {
        let deep_list = format!("{}x", "- ".repeat(MARKDOWN_MAX_LINE_NESTING + 1));
        let deep_quote = format!("{}x", "> 1. ".repeat(MARKDOWN_MAX_LINE_NESTING));
        // Pads every following line to 300 columns.
        let wide_table = format!(
            "{}\n{}\n{}",
            "|a".repeat(300),
            "|-".repeat(300),
            "x\n".repeat(400)
        );
        let quoted_wide_table = format!(
            "> {}\n> {}\n{}",
            "|a".repeat(300),
            "| :-: ".repeat(300),
            "x\n".repeat(400)
        );
        // One paragraph past the block cost, written with each kind of line
        // ending.
        let long_paragraph = "word ".repeat(4 * 1024);
        let crlf_paragraph = "word\r\n".repeat(4 * 1024);
        let cr_paragraph = "word\r".repeat(4 * 1024);
        // Many definitions and many closing brackets.
        let lookups = format!("{}\n\n{}", "[a]: b\n\n".repeat(2_000), "x] ".repeat(1_000));
        for costly in [
            deep_list.clone(),
            format!("a\r{deep_list}"),
            format!("a\r\n{deep_quote}"),
            format!("[^a]: {deep_list}"),
            format!("\u{feff}{deep_list}"),
            wide_table,
            quoted_wide_table,
            long_paragraph,
            crlf_paragraph,
            cr_paragraph,
            lookups,
            "a\n\n".repeat(MARKDOWN_MAX_SOURCE_BYTES / 3 + 1),
        ] {
            let head = &costly[..costly.len().min(80)];
            assert!(!markdown_cost_is_bounded(&costly), "{head:?}");
            let html = render_page_html(&costly, &costly, true, None);
            assert!(html.starts_with("<p>"), "{head:?}");
            assert!(!html.contains("<li>") && !html.contains("<td>"), "{head:?}");
        }
        for fine in [
            format!("{}x", "- ".repeat(MARKDOWN_MAX_LINE_NESTING)),
            "> quote\n- item\n  1. nested".to_string(),
            "| a | b |\n| - | - |\n| 1 | 2 |".to_string(),
            "-1 and 2.5 and -x".to_string(),
            // A long code block of shell pipelines is not a wide table.
            format!("```\n{}```", "a | b | c | d | e | f | g | h\n".repeat(200)),
        ] {
            assert!(markdown_cost_is_bounded(&fine), "{fine:?}");
        }
        assert_eq!(line_container_depth("  > - 1) * x"), 4);
        assert_eq!(line_container_depth(">>> x"), 3);
        assert_eq!(line_container_depth("-x"), 0);
        assert_eq!(line_container_depth("2024. was"), 1);
        assert_eq!(line_container_depth("[^a]: > - x"), 3);
        assert_eq!(line_container_depth("[^a] x"), 0);
        assert_eq!(delimiter_row_columns("| --- | :-: |"), Some(2));
        assert_eq!(delimiter_row_columns("> -|-"), Some(2));
        assert_eq!(delimiter_row_columns("| a |"), None);
        assert_eq!(delimiter_row_columns("| | |"), None);
        assert_eq!(delimiter_row_columns("---"), Some(1));
        assert_eq!(delimiter_row_columns("|-|-|-|-|"), Some(4));
    }

    /// Lines split the way markdown splits them: a `\r\n` is one line ending,
    /// not a line ending and an empty (blank) line.
    #[test]
    fn line_endings_split_like_markdown() {
        let lines: Vec<&str> = markdown_lines("a\r\nb\rc\n\nd").collect();
        assert_eq!(lines, ["a", "b", "c", "", "d"]);
        assert_eq!(markdown_lines("").collect::<Vec<_>>(), [""]);
        assert!(!is_blank_line("\u{a0}"));
        assert!(is_blank_line(" \t"));
    }

    /// A long document of ordinary markdown, near the size limit, renders as
    /// markdown.
    #[test]
    fn long_ordinary_document_renders_as_markdown() {
        let section = "## A heading\n\n\
            A paragraph with **bold**, _emphasis_, `code`, a [link](https://example.com/a) \
            and a [reference link][r]. It goes on for a while, the way paragraphs in \
            long documents do, with more words and another [link](#a-heading).\n\n\
            - item one\n- item two\n  1. nested\n\n\
            | a | b | c |\n|---|:-:|--:|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n\n\
            ```\nfn main() { println!(\"hi\"); }\n```\n\n\
            > A quote.\n\n";
        let mut text = String::new();
        while text.len() + section.len() < MARKDOWN_MAX_SOURCE_BYTES - 64 {
            text.push_str(section);
        }
        text.push_str("[r]: https://example.com/r \"A title\"\n");
        assert!(markdown_cost_is_bounded(&text));
        let html = render_page_html(&text, &text, true, None);
        assert!(html.contains("<h2 id=\"a-heading\">"), "{}", &html[..300]);
        assert!(html.contains("<table>") && html.contains("<strong>"));
        assert!(html.contains("href=\"https://example.com/r\" title=\"A title\""));
    }

    /// A definition with a long title used many times would expand to far
    /// more HTML than the limit, so it is caught before rendering.
    #[test]
    fn references_that_expand_too_far_are_shown_as_plain_text() {
        let text = format!(
            "[a]: b '{}'\n\n{}",
            "\"".repeat(10_000),
            "[a]\n\n".repeat(10_000)
        );
        assert!(markdown_cost_is_bounded(&text));
        assert!(!references_bounded(&text));
        let html = render_page_html(&text, &text, true, None);
        assert!(html.starts_with("<p>[a]: b"), "{}", &html[..100]);
        assert!(html.len() < 8 * text.len());

        // Many reference links to short titled definitions are fine, although
        // the cheap bound alone would reject them (a title may run to the end
        // of the run, and the definitions are one run).
        let mut many = String::new();
        for i in 0..150 {
            many.push_str(&format!("See [this][r{i}] and [that][r{i}].\n\n"));
        }
        for i in 0..150 {
            many.push_str(&format!("[r{i}]: https://example.com/{i} \"Title {i}\"\n"));
        }
        assert!(markdown_cost_is_bounded(&many));
        let cheap = many.matches(']').count() * (longest_possible_definition(&many) * 6 + 64);
        assert!(cheap > MARKDOWN_MAX_HTML_BYTES, "{cheap}");
        assert!(references_bounded(&many));
        let html = render_page_html(&many, &many, true, None);
        assert!(html.contains("href=\"https://example.com/149\" title=\"Title 149\""));

        // Untitled definitions have a tight cheap bound, so a long document of
        // them needs no second parse.
        let mut untitled = String::new();
        for i in 0..300 {
            untitled.push_str(&format!("See [this][r{i}].\n\n"));
        }
        for i in 0..300 {
            untitled.push_str(&format!("[r{i}]: https://example.com/{i}\n"));
        }
        assert!(longest_possible_definition(&untitled) < 100);
        assert!(render_page_html(&untitled, &untitled, true, None).contains("/299\""));
    }

    /// Text not rendered as markdown is escaped, keeps its line breaks, and
    /// shows the content as written (here, before page-link resolution).
    #[test]
    fn plain_text_is_escaped_with_line_breaks() {
        let costly = format!("<b>[[Page]]</b> & \"x\"\r\n{}", "- ".repeat(40));
        let html = render_page_html(&costly, "<b>[[Page]]</b>\r\nsecond", true, None);
        assert_eq!(html, "<p>&lt;b&gt;[[Page]]&lt;/b&gt;<br />\nsecond</p>");
        assert!(!render_page_html(&costly, &costly, true, None).contains("<b>"));
    }
}
