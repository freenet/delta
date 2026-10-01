//! Markdown rendering for page content, for both the page view and the
//! editor's live preview.
//!
//! Page content is written by other people, so the work spent rendering it is
//! bounded: text past the limits below is shown as escaped plain text instead
//! of markdown. The `markdown` crate is pinned to a patched build in the root
//! Cargo.toml (`[patch.crates-io]`).
//!
//! This module is the only code that calls the `markdown` crate; a test in
//! it fails if anything else does, or if the page view or editor preview
//! stops rendering through it.
//!
//! Adapted from River's message rendering (`ui/src/components/conversation.rs`
//! in freenet/river), with limits sized for long documents rather than chat
//! messages.

use super::page_view::{finalize_anchors, inject_heading_ids};

/// Bounds on the work spent rendering one text as markdown. Text past any of
/// them is shown as plain text. The values used are `LIMITS`; tests use
/// smaller ones so their inputs stay tiny.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// Longest text rendered as markdown, in bytes.
    source_bytes: usize,
    /// Most block containers (`>`, `-`, `1.` ...) opened at the start of one
    /// line.
    line_nesting: usize,
    /// Most block container markers in the whole text: the sum over its lines
    /// of the containers each opens (see `line_container_depth`).
    container_markers: usize,
    /// Bound on the sum, over each run of non-blank lines, of the run's
    /// length squared, in bytes squared.
    block_cost: u64,
    /// Most table cells the HTML can contain.
    table_cells: usize,
    /// Bound on `]:` occurrences times `]` occurrences.
    reference_lookups: usize,
    /// Longest HTML kept from rendering markdown, in bytes. It also bounds
    /// the HTML that references can expand to before rendering (see
    /// `references_are_bounded`).
    html_bytes: usize,
}

/// The limits page content is rendered under.
///
/// - `source_bytes`: 32 times River's message limit, long enough for a long
///   document (the largest markdown file across the Freenet and mediator
///   repositories is under 60 KiB). Ordinary text this long renders in about
///   0.1 s; the slowest text found that passes every limit takes about 0.7 s
///   natively and 1 s in the browser. (The patched `markdown` build parses
///   in time linear in a document's length apart from the inline cost below;
///   the 1.0.0 release is quadratic, so this limit is only safe with it.)
/// - `line_nesting`: the same as River. Parser time grows faster than
///   linearly with nesting.
/// - `container_markers`: the most any real markdown file measured opens is
///   about 700, so this leaves a wide margin. A very long checklist or list
///   (thousands of items) is shown as plain text.
/// - `block_cost`: inline parsing (emphasis, brackets, code spans, inline
///   HTML) can take time quadratic in the length of a paragraph, and a
///   paragraph never spans a blank line. This allows a single 12 KiB run, or
///   36 runs of 2 KiB, and so on. The largest real document measured (a
///   40 KiB design document with a 9 KiB run) uses 70% of it, and 99% of the
///   markdown files measured use under a quarter.
/// - `table_cells`: a table's body rows are padded to the width of its
///   delimiter row, so a wide delimiter row followed by many short lines
///   expands to columns times rows cells.
/// - `reference_lookups`: each closing bracket is checked against every
///   definition, so many of both is quadratic.
/// - `html_bytes`: a backstop on output size.
const LIMITS: Limits = Limits {
    source_bytes: 128 * 1024,
    line_nesting: 16,
    container_markers: 4 * 1024,
    block_cost: 12 * 1024 * 12 * 1024,
    table_cells: 64 * 1024,
    reference_lookups: 1024 * 1024,
    html_bytes: 4 * 1024 * 1024,
};

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
    render_page_html_with(&LIMITS, text, plain, rewrite_freenet_hrefs, own_contract_id)
}

/// Render a page's stored content for the page view.
///
/// `resolve_links(content, max_len)` rewrites Delta's `[[...]]` page links
/// into markdown, or returns `None` if the result would be longer than
/// `max_len`. It is only called for content within the size limit, and its
/// result goes through the same limits as any other text. Content that is
/// too long, or too costly to render, is shown as written, as plain text.
pub(super) fn render_page_view_html(
    content: &str,
    resolve_links: impl FnOnce(&str, usize) -> Option<String>,
    rewrite_freenet_hrefs: bool,
    own_contract_id: Option<&str>,
) -> String {
    render_page_view_html_with(
        &LIMITS,
        content,
        resolve_links,
        rewrite_freenet_hrefs,
        own_contract_id,
    )
}

fn render_page_view_html_with(
    limits: &Limits,
    content: &str,
    resolve_links: impl FnOnce(&str, usize) -> Option<String>,
    rewrite_freenet_hrefs: bool,
    own_contract_id: Option<&str>,
) -> String {
    // Checked before resolving links, which is work in proportion to the
    // text. Resolved text longer than this would be plain text anyway.
    if content.len() > limits.source_bytes {
        return plain_text_to_html(content);
    }
    match resolve_links(content, limits.source_bytes) {
        Some(resolved) => render_page_html_with(
            limits,
            &resolved,
            content,
            rewrite_freenet_hrefs,
            own_contract_id,
        ),
        None => plain_text_to_html(content),
    }
}

fn render_page_html_with(
    limits: &Limits,
    text: &str,
    plain: &str,
    rewrite_freenet_hrefs: bool,
    own_contract_id: Option<&str>,
) -> String {
    let Some(html) = render_gfm_bounded(text, limits) else {
        return plain_text_to_html(plain);
    };
    let html = inject_heading_ids(&html);
    let html = finalize_anchors(&html, rewrite_freenet_hrefs, own_contract_id);
    if html.len() > limits.html_bytes {
        return plain_text_to_html(plain);
    }
    html
}

/// GFM HTML for `text`, or `None` if it is too costly to render.
fn render_gfm_bounded(text: &str, limits: &Limits) -> Option<String> {
    let cost = markdown_parse_cost(text, limits)?;
    if !references_are_bounded(text, &cost, limits) {
        return None;
    }
    // GFM without MDX never fails to parse.
    let html = markdown::to_html_with_options(text, &markdown::Options::gfm()).ok()?;
    (html.len() <= limits.html_bytes).then_some(html)
}

/// The work a text would take to parse, as counted against `Limits`.
#[derive(Debug, Default, PartialEq)]
struct ParseCost {
    /// See `Limits::block_cost`.
    block: u64,
    /// See `Limits::container_markers`.
    container_markers: usize,
    /// The most containers opened at the start of any one line.
    deepest_line: usize,
    /// See `Limits::table_cells`.
    table_cells: usize,
    /// `]:` occurrences times `]` occurrences.
    reference_lookups: usize,
}

impl ParseCost {
    /// Measure `text`, in time linear in its length.
    fn of(text: &str) -> Self {
        let closing_brackets = text.matches(']').count();
        let definitions = text.matches("]:").count();
        let mut cost = ParseCost {
            reference_lookups: closing_brackets.saturating_mul(definitions),
            ..ParseCost::default()
        };
        // The parser skips one byte order mark at the very start, so a marker
        // just after it still opens a container. Any other U+FEFF is ordinary
        // text to the parser, and so it is here.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        let mut run_bytes: u64 = 0;
        let mut run_table_columns: usize = 0;
        for line in markdown_lines(text) {
            let depth = line_container_depth(line);
            cost.deepest_line = cost.deepest_line.max(depth);
            cost.container_markers = cost.container_markers.saturating_add(depth);
            if is_blank_line(line) {
                cost.block = cost.block.saturating_add(run_bytes * run_bytes);
                run_bytes = 0;
                run_table_columns = 0;
                continue;
            }
            run_bytes += line.len() as u64 + 1;
            if let Some(columns) = delimiter_row_columns(line) {
                run_table_columns = run_table_columns.max(columns);
            }
            // Every later line of the run may be a row of a table this wide.
            cost.table_cells = cost.table_cells.saturating_add(run_table_columns);
        }
        cost.block = cost.block.saturating_add(run_bytes * run_bytes);
        cost
    }

    fn is_within(&self, limits: &Limits) -> bool {
        self.deepest_line <= limits.line_nesting
            && self.container_markers <= limits.container_markers
            && self.block <= limits.block_cost
            && self.table_cells <= limits.table_cells
            && self.reference_lookups <= limits.reference_lookups
    }
}

/// `text`'s parse cost if it is cheap enough to parse as markdown, or `None`.
/// The size limit is checked first, so measuring is bounded too.
fn markdown_parse_cost(text: &str, limits: &Limits) -> Option<ParseCost> {
    if text.len() > limits.source_bytes {
        return None;
    }
    let cost = ParseCost::of(text);
    cost.is_within(limits).then_some(cost)
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

/// Bytes of HTML per byte of a copied destination or title (`"` becomes
/// `&quot;`).
const REFERENCE_HTML_PER_BYTE: usize = 6;
/// Bytes of HTML per reference for the tags around it.
const REFERENCE_HTML_PER_REFERENCE: usize = 64;

/// Whether the HTML that link and image references expand to is bounded.
///
/// Each reference copies its definition's destination and title into the
/// HTML, so a few long definitions used many times expand a page
/// quadratically. A cheap bound comes first: every reference ends in `]`, and
/// see `longest_possible_definition`. Only if that bound is too high is the
/// text parsed to count exactly, and since that parses it a second time, only
/// text within half of the size and block cost limits gets that far.
fn references_are_bounded(text: &str, cost: &ParseCost, limits: &Limits) -> bool {
    let closing_brackets = text.matches(']').count();
    let longest_definition = longest_possible_definition(text);
    let cheap_bound = closing_brackets.saturating_mul(
        longest_definition
            .saturating_mul(REFERENCE_HTML_PER_BYTE)
            .saturating_add(REFERENCE_HTML_PER_REFERENCE),
    );
    if cheap_bound <= limits.html_bytes {
        return true;
    }
    if text.len() > limits.source_bytes / 2 || cost.block > limits.block_cost / 2 {
        return false;
    }
    exact_reference_expansion(text, longest_definition)
        .is_some_and(|expansion| expansion.html_bytes <= limits.html_bytes)
}

/// What walking a syntax tree for references found.
#[derive(Debug, PartialEq)]
struct ReferenceExpansion {
    /// Bytes of HTML the references expand to (an over-estimate).
    html_bytes: usize,
    /// Tree nodes visited.
    nodes: usize,
}

/// Parse `text` to a syntax tree and count what its references expand to,
/// then drop the tree. `None` only if the parser reports an error, which it
/// does not for GFM.
fn exact_reference_expansion(text: &str, longest_definition: usize) -> Option<ReferenceExpansion> {
    let tree = markdown::to_mdast(text, &markdown::ParseOptions::gfm()).ok()?;
    let expansion = reference_expansion(&tree, longest_definition);
    drop_mdast(tree);
    Some(expansion)
}

/// Count what the references in `root` expand to, visiting each node once.
fn reference_expansion(
    root: &markdown::mdast::Node,
    fallback_definition: usize,
) -> ReferenceExpansion {
    use markdown::mdast::Node;
    use std::collections::HashMap;

    let mut definitions: HashMap<&str, usize> = HashMap::new();
    let mut references: Vec<&str> = Vec::new();
    let mut nodes = 0;
    // Iterative, not recursive: nesting depth is chosen by whoever wrote the
    // text, and a recursive walk of a deep tree overflows the stack. Pre-order,
    // so definitions are seen in document order and the first one wins, as it
    // does when rendering.
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        nodes += 1;
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
    let html_bytes = references.iter().fold(0usize, |total, id| {
        // Every reference in the tree resolved to a definition; if the two
        // disagree on an identifier, assume the longest possible one.
        let len = definitions.get(id).copied().unwrap_or(fallback_definition);
        total.saturating_add(
            len.saturating_mul(REFERENCE_HTML_PER_BYTE)
                .saturating_add(REFERENCE_HTML_PER_REFERENCE),
        )
    });
    ReferenceExpansion { html_bytes, nodes }
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

    /// Small limits, so tests can reach each one with a tiny input. They keep
    /// the production ratios that matter: the reference gate is at half the
    /// size and block cost limits.
    const SMALL: Limits = Limits {
        source_bytes: 256,
        line_nesting: 4,
        container_markers: 16,
        block_cost: 64 * 64,
        table_cells: 32,
        reference_lookups: 12,
        html_bytes: 4 * 1024,
    };

    fn cost(text: &str) -> ParseCost {
        ParseCost::of(text)
    }

    fn renders_as_markdown(limits: &Limits, text: &str) -> bool {
        render_gfm_bounded(text, limits).is_some()
    }

    fn assert_plain(limits: &Limits, text: &str) {
        let html = render_page_html_with(limits, text, text, true, None);
        assert_eq!(html, plain_text_to_html(text), "{text:?}");
    }

    /// Both rendering paths (page view and editor preview share
    /// `render_page_html`), with and without the gateway href rewrite.
    fn render_every_way(text: &str) {
        for (rewrite, own) in [(true, None), (false, Some(OWN_ID)), (true, Some(OWN_ID))] {
            let _ = render_page_html(text, text, rewrite, own);
        }
    }

    /// Inputs the unpatched `markdown` crate panics on (see
    /// `[patch.crates-io]` in the root Cargo.toml): the regression inputs of
    /// River's `malformed_markdown_renders_without_panicking`. Each goes
    /// through both of the crate's code paths this module uses: HTML
    /// rendering, and the syntax tree built for the exact reference count.
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
                let walked = std::panic::catch_unwind(|| exact_reference_expansion(&text, 0));
                assert!(
                    matches!(walked, Ok(Some(_))),
                    "building the syntax tree of {text:?} failed"
                );
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
            let rendered = std::panic::catch_unwind(|| {
                render_every_way(&text);
                exact_reference_expansion(&text, 0)
            });
            assert!(rendered.is_ok(), "rendering {text:?} panicked");
        }
    }

    /// The AST helpers walk and drop a tree without recursion, so a deep tree
    /// cannot overflow the stack. Runs on a small stack to leave a margin
    /// below wasm's 1 MiB. The walk visits each node once: the count grows
    /// with the nesting exactly.
    #[test]
    fn deep_markdown_trees_are_walked_without_recursion() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let walk = |depth: usize| {
                    let deep = format!("{}[a]\n\n[a]: b", ">".repeat(depth));
                    exact_reference_expansion(&deep, 0).unwrap()
                };
                // Root, `depth` quotes, a paragraph, a reference, its text,
                // and the definition.
                for depth in [1, 2, 4, 5_000] {
                    let expansion = walk(depth);
                    assert_eq!(expansion.html_bytes, 6 + 64);
                    assert_eq!(expansion.nodes, depth + 5, "{depth}");
                }
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
            if text.len() + line.len() + 6_100 > LIMITS.source_bytes / 2 {
                break;
            }
            text.push_str(&line);
            depth += 1;
        }
        // A definition followed, in the same run, by a long paragraph, so the
        // cheap reference bound fails and the tree is built and walked.
        text.push_str(&format!("[a]: b\n{}\n", "word ".repeat(1_200)));
        assert!(depth > 200, "{depth}");
        assert!(markdown_parse_cost(&text, &LIMITS).is_some());
        let cheap = text.matches(']').count() * (longest_possible_definition(&text) * 6 + 64);
        assert!(cheap > LIMITS.html_bytes, "{cheap}");
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

    /// The values the limits are set to. Changing one changes what readers
    /// see, so it should be a deliberate edit here too.
    #[test]
    fn production_limits() {
        let Limits {
            source_bytes,
            line_nesting,
            container_markers,
            block_cost,
            table_cells,
            reference_lookups,
            html_bytes,
        } = LIMITS;
        assert_eq!(source_bytes, 131_072);
        assert_eq!(line_nesting, 16);
        assert_eq!(container_markers, 4_096);
        assert_eq!(block_cost, 12_288 * 12_288);
        assert_eq!(table_cells, 65_536);
        assert_eq!(reference_lookups, 1_048_576);
        assert_eq!(html_bytes, 4_194_304);
        // And `render_page_html` applies them.
        let deep = format!("{}x", "- ".repeat(line_nesting + 1));
        assert_eq!(
            render_page_html(&deep, &deep, true, None),
            plain_text_to_html(&deep)
        );
        let fine = format!("{}x", "- ".repeat(line_nesting));
        assert!(render_page_html(&fine, &fine, true, None).contains("<li>"));
    }

    /// The source size limit is exact.
    #[test]
    fn size_limit() {
        let at = "a".repeat(SMALL.source_bytes);
        let over = "a".repeat(SMALL.source_bytes + 1);
        let limits = Limits {
            block_cost: u64::MAX,
            ..SMALL
        };
        assert!(renders_as_markdown(&limits, &at));
        assert!(!renders_as_markdown(&limits, &over));
        assert_plain(&limits, &over);
    }

    /// Containers opened on one line are counted however the line is
    /// reached, and the per-line limit is exact.
    #[test]
    fn line_nesting_limit() {
        let deep_list = format!("{}x", "- ".repeat(SMALL.line_nesting + 1));
        let deep_quote = format!("{}x", "> 1. ".repeat(SMALL.line_nesting / 2 + 1));
        for costly in [
            deep_list.clone(),
            format!("a\r{deep_list}"),
            format!("a\r\n{deep_quote}"),
            format!("[^a]: {deep_list}"),
            format!("\u{feff}{deep_list}"),
        ] {
            assert!(
                cost(&costly).deepest_line > SMALL.line_nesting,
                "{costly:?}"
            );
            assert_plain(&SMALL, &costly);
        }
        let at = format!("{}x", "- ".repeat(SMALL.line_nesting));
        assert_eq!(cost(&at).deepest_line, SMALL.line_nesting);
        assert!(renders_as_markdown(&SMALL, &at));

        assert_eq!(line_container_depth("  > - 1) * x"), 4);
        assert_eq!(line_container_depth(">>> x"), 3);
        assert_eq!(line_container_depth("-x"), 0);
        assert_eq!(line_container_depth("2024. was"), 1);
        assert_eq!(line_container_depth("[^a]: > - x"), 3);
        assert_eq!(line_container_depth("[^a] x"), 0);
        assert_eq!(line_container_depth("-1 and 2.5 and -x"), 0);
    }

    /// The whole-text container count is the sum of each line's, so it grows
    /// linearly with the text, and its limit is exact.
    #[test]
    fn container_marker_limit() {
        for lines in [1, 2, 4, 8] {
            assert_eq!(cost(&"- a\n".repeat(lines)).container_markers, lines);
            assert_eq!(cost(&"> - a\n".repeat(lines)).container_markers, 2 * lines);
            assert_eq!(cost(&"- a\n\n".repeat(lines)).container_markers, lines);
        }
        let limits = Limits {
            block_cost: u64::MAX,
            ..SMALL
        };
        let at = "- a\n".repeat(limits.container_markers);
        let over = "- a\n".repeat(limits.container_markers + 1);
        assert!(renders_as_markdown(&limits, &at));
        assert!(!renders_as_markdown(&limits, &over));
        assert_plain(&limits, &over);
        // Spread over lines that each stay well within the per-line limit.
        let quoted = "> > a\n\n".repeat(limits.container_markers / 2 + 1);
        assert!(cost(&quoted).deepest_line <= limits.line_nesting);
        assert!(!renders_as_markdown(&limits, &quoted));
    }

    /// A byte order mark is skipped only at the very start, as the parser
    /// does. Anywhere else it is text, and counts as text.
    #[test]
    fn byte_order_mark_is_skipped_only_at_the_start() {
        // A leading one does not hide the markers after it.
        let deep = format!("\u{feff}{}x", "- ".repeat(SMALL.line_nesting + 1));
        assert_eq!(cost(&deep).deepest_line, SMALL.line_nesting + 1);
        assert_plain(&SMALL, &deep);
        assert!(markdown::to_html("\u{feff}- x").contains("<li>"));

        // A second one is text: no list opens, for the parser or the count.
        assert_eq!(cost("\u{feff}\u{feff}- x").container_markers, 0);
        assert!(!markdown::to_html("\u{feff}\u{feff}- x").contains("<li>"));

        // Marks inside a paragraph count toward its length (3 bytes each).
        for marks in [1, 2, 4, 8] {
            let text = format!("a{}", "\u{feff}".repeat(marks));
            let run = (1 + 3 * marks + 1) as u64;
            assert_eq!(cost(&text).block, run * run, "{marks}");
        }
        let marks = "\u{feff}".repeat(30);
        assert!(cost(&marks).block > SMALL.block_cost);
        assert_plain(&SMALL, &marks);
    }

    /// Block cost is the sum of each run's length squared: quadratic in a
    /// run, linear in the number of runs. The limit is exact.
    #[test]
    fn block_cost_limit() {
        for lines in [1, 2, 4] {
            let run = 3 * lines as u64;
            assert_eq!(cost(&"ab\n".repeat(lines)).block, run * run);
            assert_eq!(cost(&"ab\r\n".repeat(lines)).block, run * run);
            assert_eq!(cost(&"ab\r".repeat(lines)).block, run * run);
            assert_eq!(cost(&"ab\n\n".repeat(lines)).block, 9 * lines as u64);
        }
        // 63 bytes and a line ending: exactly the 64 x 64 limit.
        let at = "a".repeat(63);
        let over = "a".repeat(64);
        assert_eq!(cost(&at).block, SMALL.block_cost);
        assert!(renders_as_markdown(&SMALL, &at));
        assert!(!renders_as_markdown(&SMALL, &over));
        // Each line ending, however written, keeps the run going.
        for ending in ["\n", "\r\n", "\r"] {
            let paragraph = format!("word{ending}").repeat(16);
            assert!(cost(&paragraph).block > SMALL.block_cost, "{ending:?}");
            assert_plain(&SMALL, &paragraph);
        }
        // The same text split by blank lines is fine.
        assert!(renders_as_markdown(&SMALL, &"word\n\n".repeat(16)));
        assert!(!is_blank_line("\u{a0}"));
        assert!(is_blank_line(" \t"));
    }

    /// Lines split the way markdown splits them: a `\r\n` is one line ending,
    /// not a line ending and an empty (blank) line.
    #[test]
    fn line_endings_split_like_markdown() {
        let lines: Vec<&str> = markdown_lines("a\r\nb\rc\n\nd").collect();
        assert_eq!(lines, ["a", "b", "c", "", "d"]);
        assert_eq!(markdown_lines("").collect::<Vec<_>>(), [""]);
    }

    /// Table cells are the widest delimiter row so far times the rows after
    /// it in the run, and the limit is exact.
    #[test]
    fn table_cell_limit() {
        // A 4-column table with `rows` body rows.
        let table = |rows: usize| format!("|a|b|c|d|\n|-|-|-|-|\n{}", "x\n".repeat(rows));
        for rows in [0, 1, 2, 4] {
            // The delimiter row itself counts as one row.
            assert_eq!(cost(&table(rows)).table_cells, 4 * (rows + 1));
        }
        let limits = Limits {
            block_cost: u64::MAX,
            ..SMALL
        };
        assert!(renders_as_markdown(&limits, &table(7)));
        assert!(!renders_as_markdown(&limits, &table(8)));
        assert_plain(&limits, &table(8));
        let quoted = format!("> |a|b|\n> | :-: | :-: |\n{}", "x\n".repeat(16));
        assert!(!renders_as_markdown(&limits, &quoted));
        // A code block of shell pipelines is not a table.
        let pipes = format!("```\n{}```", "a | b | c\n".repeat(8));
        assert_eq!(cost(&pipes).table_cells, 0);

        assert_eq!(delimiter_row_columns("| --- | :-: |"), Some(2));
        assert_eq!(delimiter_row_columns("> -|-"), Some(2));
        assert_eq!(delimiter_row_columns("| a |"), None);
        assert_eq!(delimiter_row_columns("| | |"), None);
        assert_eq!(delimiter_row_columns("---"), Some(1));
        assert_eq!(delimiter_row_columns("|-|-|-|-|"), Some(4));
    }

    /// `]:` count times `]` count, with an exact limit.
    #[test]
    fn reference_lookup_limit() {
        // `]:` itself contains a `]`.
        assert_eq!(cost("]:]:]:]").reference_lookups, 3 * 4);
        assert_eq!(cost("]:]:]:]]").reference_lookups, 3 * 5);
        assert_eq!(cost("]]]]").reference_lookups, 0);
        assert!(renders_as_markdown(&SMALL, "]:]:]:]"));
        assert!(!renders_as_markdown(&SMALL, "]:]:]:]]"));
        assert_plain(&SMALL, "]:]:]:]]");
    }

    /// The exact reference count: each reference costs its definition's
    /// destination and title, six times over, plus 64 bytes. Only definitions
    /// that something refers to count, and the first definition of a label
    /// wins.
    #[test]
    fn exact_reference_count() {
        let count = |text: &str| exact_reference_expansion(text, 0).unwrap().html_bytes;
        // Destination 5 bytes, title 3.
        let one = (5 + 3) * 6 + 64;
        for refs in [1, 2, 4] {
            let text = format!("[a]: bbbbb 'ttt'\n\n{}", "[a] ".repeat(refs));
            assert_eq!(count(&text), refs * one, "{refs}");
        }
        assert_eq!(count("[a]: bbbbb 'ttt'\n\n[z]"), 0);
        assert_eq!(count("[a]: b\n\n[a]: ccccc\n\n[a]"), 6 + 64);
        assert_eq!(count("[a]: bb\n\n![x][a]"), 2 * 6 + 64);
    }

    /// The exact count is used only when the cheap bound fails, and only for
    /// text within half the size and block cost limits.
    #[test]
    fn exact_reference_count_is_gated_at_half_the_limits() {
        // A titled definition followed by more text in its run: the cheap
        // bound assumes the title may run to the end of the run.
        let text_of_len = |len: usize| {
            let head = "[a]: b 't'\nx";
            format!("{head}{}\n\n[a]", "y".repeat(len - head.len() - 5))
        };
        let limits = Limits {
            html_bytes: 64,
            block_cost: u64::MAX,
            ..SMALL
        };
        let half = limits.source_bytes / 2;
        let at = text_of_len(half);
        let over = text_of_len(half + 1);
        assert_eq!((at.len(), over.len()), (half, half + 1));
        for text in [&at, &over] {
            let cheap = text.matches(']').count() * (longest_possible_definition(text) * 6 + 64);
            assert!(cheap > limits.html_bytes);
            // Destination and title are one byte each.
            assert_eq!(count_html(text), 2 * 6 + 64);
        }
        let cost_at = markdown_parse_cost(&at, &limits).unwrap();
        let cost_over = markdown_parse_cost(&over, &limits).unwrap();
        // The exact count (76 bytes) is over 64, so lift that for this check.
        let roomy = Limits {
            html_bytes: 80,
            ..limits
        };
        assert!(references_are_bounded(&at, &cost_at, &roomy));
        assert!(!references_are_bounded(&over, &cost_over, &roomy));
        assert!(!references_are_bounded(&at, &cost_at, &limits));

        // The same at half the block cost.
        let block_limits = Limits {
            block_cost: 2 * cost_at.block,
            ..roomy
        };
        assert!(references_are_bounded(&at, &cost_at, &block_limits));
        let block_limits = Limits {
            block_cost: 2 * cost_at.block - 1,
            ..roomy
        };
        assert!(!references_are_bounded(&at, &cost_at, &block_limits));

        fn count_html(text: &str) -> usize {
            exact_reference_expansion(text, 0).unwrap().html_bytes
        }
    }

    /// The cheap reference bound lets many short untitled definitions through
    /// without a second parse, and the exact count catches a long title used
    /// many times.
    #[test]
    fn reference_expansion_falls_back_to_plain_text() {
        let limits = Limits {
            block_cost: u64::MAX,
            ..SMALL
        };
        // 4 references to a 30-byte title: 4 x 244 bytes.
        let text = format!("[a]: b '{}'\n\n{}", "\"".repeat(29), "[a]\n\n".repeat(4));
        assert!(text.len() <= limits.source_bytes / 2);
        let expanded = exact_reference_expansion(&text, 0).unwrap().html_bytes;
        assert_eq!(expanded, 4 * (30 * 6 + 64));
        let tight = Limits {
            html_bytes: expanded - 1,
            ..limits
        };
        assert!(!renders_as_markdown(&tight, &text));
        assert_plain(&tight, &text);

        let untitled = "See [r1] and [r2].\n\n[r1]: /a\n[r2]: /b\n";
        assert!(longest_possible_definition(untitled) < 16);
        let html = render_page_html_with(&limits, untitled, untitled, true, None);
        assert!(html.contains("href=\"/b\""), "{html}");
    }

    /// The HTML is capped twice: as the parser writes it, and after heading
    /// ids and link attributes are added. Each cap is exact.
    #[test]
    fn html_output_caps() {
        let text = "# Head\n\n[x](https://e.example)";
        let raw = markdown::to_html_with_options(text, &markdown::Options::gfm()).unwrap();
        let finished = render_page_html_with(&SMALL, text, text, true, None);
        assert!(finished.len() > raw.len(), "{finished}");
        assert!(finished.contains("target=\"_blank\"") && finished.contains("id=\"head\""));

        let with_cap = |html_bytes| Limits {
            html_bytes,
            ..SMALL
        };
        let render = |cap| render_page_html_with(&with_cap(cap), text, text, true, None);
        // Parser output over the cap.
        assert_eq!(render(raw.len() - 1), plain_text_to_html(text));
        assert!(render_gfm_bounded(text, &with_cap(raw.len() - 1)).is_none());
        assert!(render_gfm_bounded(text, &with_cap(raw.len())).is_some());
        // Parser output within the cap, finished HTML over it.
        assert_eq!(render(raw.len()), plain_text_to_html(text));
        assert_eq!(render(finished.len() - 1), plain_text_to_html(text));
        assert_eq!(render(finished.len()), finished);
    }

    /// A long document of ordinary markdown renders as markdown. Its cost
    /// grows linearly with its length, so at the production size limit it
    /// stays within every other production limit.
    #[test]
    fn ordinary_markdown_renders_and_scales_linearly() {
        let section = "## A heading\n\n\
            A paragraph with **bold**, _emphasis_, `code`, a [link](https://example.com/a) \
            and a [reference link][r]. It goes on for a while, the way paragraphs in \
            long documents do, with more words and another [link](#a-heading).\n\n\
            - item one\n- item two\n  1. nested\n- [ ] a task\n\n\
            | a | b | c |\n|---|:-:|--:|\n| 1 | 2 | 3 |\n| 4 | 5 | 6 |\n\n\
            ```\nfn main() { println!(\"hi\"); }\n```\n\n\
            > A quote.\n\n";
        let definition = "[r]: https://example.com/r \"A title\"\n";
        let doc = |sections: usize| format!("{}{definition}", section.repeat(sections));

        let one = cost(&doc(1));
        let two = cost(&doc(2));
        let four = cost(&doc(4));
        assert_eq!(four.container_markers, 4 * one.container_markers);
        assert_eq!(four.table_cells, 4 * one.table_cells);
        // Runs do not grow, so block cost is linear (the last run differs).
        assert_eq!(four.block - two.block, 2 * (two.block - one.block));

        let sections_at_limit = LIMITS.source_bytes / section.len();
        let per_section = |field: fn(&ParseCost) -> u64| field(&two) - field(&one);
        let at_limit = |field: fn(&ParseCost) -> u64| {
            field(&one) + per_section(field) * sections_at_limit as u64
        };
        assert!(at_limit(|c| c.block) <= LIMITS.block_cost / 2);
        assert!(at_limit(|c| c.container_markers as u64) <= LIMITS.container_markers as u64);
        assert!(at_limit(|c| c.table_cells as u64) <= LIMITS.table_cells as u64);
        assert!(at_limit(|c| c.deepest_line as u64) <= LIMITS.line_nesting as u64);

        let text = doc(4);
        let html = render_page_html(&text, &text, true, None);
        assert!(html.contains("<h2 id=\"a-heading\">"), "{}", &html[..300]);
        assert!(html.contains("<table>") && html.contains("<strong>"));
        assert!(html.contains("href=\"https://example.com/r\" title=\"A title\""));
    }

    /// The page view checks the size before resolving page links, and shows
    /// the content as written if resolving would make it too long.
    #[test]
    fn page_view_checks_size_before_resolving_links() {
        let over = "a".repeat(SMALL.source_bytes + 1);
        let html = render_page_view_html_with(
            &SMALL,
            &over,
            |_, _| panic!("resolved links in text over the size limit"),
            true,
            None,
        );
        assert_eq!(html, plain_text_to_html(&over));

        let content = "[[Page]] <b>";
        let mut max_seen = None;
        let html = render_page_view_html_with(
            &SMALL,
            content,
            |text, max| {
                assert_eq!(text, content);
                max_seen = Some(max);
                None
            },
            true,
            None,
        );
        assert_eq!(max_seen, Some(SMALL.source_bytes));
        assert_eq!(html, "<p>[[Page]] &lt;b&gt;</p>");

        // Resolved text goes through the limits, and the plain text shown is
        // the content as written.
        let deep = format!("{}x", "- ".repeat(SMALL.line_nesting + 1));
        let html =
            render_page_view_html_with(&SMALL, "[[P]]", |_, _| Some(deep.clone()), true, None);
        assert_eq!(html, "<p>[[P]]</p>");
        let html =
            render_page_view_html_with(&SMALL, "[[P]]", |_, _| Some("[P](#p)".into()), true, None);
        assert!(html.contains("<a href=\"#p\">P</a>"), "{html}");
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

    /// Every use of the `markdown` crate goes through this module, and both
    /// the page view and the editor preview render through it. A component
    /// that called the crate directly, or stopped calling the bounded entry
    /// points, fails here.
    #[test]
    fn page_view_and_editor_render_through_this_module() {
        /// Source with `//` comments removed, so a comment mentioning a call
        /// does not count as one.
        fn code(source: &str) -> String {
            source
                .lines()
                .map(|line| line.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n")
        }
        fn uses_crate(code: &str, krate: &str) -> bool {
            code.match_indices(&format!("{krate}::")).any(|(i, _)| {
                !code[..i]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
        }
        /// The body of `fn name` in `code`: from its signature to the first
        /// line that closes an item at the left margin.
        fn body<'a>(code: &'a str, name: &str) -> &'a str {
            let start = code
                .find(&format!("fn {name}("))
                .unwrap_or_else(|| panic!("no fn {name}"));
            let end = code[start..].find("\n}").expect("item end");
            &code[start..start + end]
        }
        let module = concat!("mark", "down_render::");

        let editor = code(include_str!("editor.rs"));
        let editor_fn = body(&editor, "Editor");
        assert!(
            editor_fn.contains(&format!("{module}render_page_html(")),
            "the editor preview no longer renders through render_page_html"
        );

        let page_view = code(include_str!("page_view.rs"));
        assert!(
            body(&page_view, "PageView").contains("render_markdown("),
            "the page view no longer renders through render_markdown"
        );
        assert!(
            body(&page_view, "render_markdown").contains("page_content_html("),
            "render_markdown no longer renders through page_content_html"
        );
        assert!(
            body(&page_view, "page_content_html")
                .contains(&format!("{module}render_page_view_html(")),
            "page_content_html no longer renders through render_page_view_html"
        );

        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut dirs = vec![std::path::PathBuf::from(src)];
        let mut scanned = 0;
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && !path.ends_with("components/markdown_render.rs")
                {
                    let source = std::fs::read_to_string(&path).expect("read");
                    assert!(
                        !uses_crate(&code(&source), "markdown"),
                        "{} uses the markdown crate directly",
                        path.display()
                    );
                    scanned += 1;
                }
            }
        }
        assert!(scanned > 10, "scanned only {scanned} files");
        assert!(uses_crate("x = markdown::to_html(t)", "markdown"));
        assert!(!uses_crate("super::render_markdown::x()", "markdown"));
    }

    /// The patched `markdown` build is the one compiled in. The parser fixes
    /// are tested above; the last commit (linear-time parsing) changes no
    /// output, so this pin is what keeps it.
    #[test]
    fn patched_markdown_build_is_pinned() {
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../Cargo.lock"))
            .expect("read Cargo.lock");
        let rev = "c0646ed72010008a3ea92bf52be43082a09bdd12";
        let source = format!("git+https://github.com/freenet/markdown-rs?rev={rev}#{rev}");
        let packages: Vec<&str> = lock
            .split("[[package]]")
            .filter(|p| p.contains("\nname = \"markdown\"\n"))
            .collect();
        assert_eq!(packages.len(), 1, "one markdown package in Cargo.lock");
        assert!(packages[0].contains(&source), "{}", packages[0]);
    }
}
