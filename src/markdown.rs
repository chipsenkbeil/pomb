use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::LazyLock;

use ammonia::Builder;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};
use rand::Rng;
use sha2::{Digest, Sha256};

/// Allowlist for rendered markdown: no `input`, `img`, `style` or script-capable markup.
static SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::default();
    builder
        .rm_tags(["img"])
        .add_tags(["del", "table", "thead", "tbody", "tr", "th", "td"])
        .add_tag_attributes("th", ["align"])
        .add_tag_attributes("td", ["align"])
        .set_tag_attribute_value("a", "target", "_blank")
        .link_rel(Some("noopener noreferrer"))
        .url_schemes(HashSet::from(["http", "https", "mailto"]));
    builder
});

/// Represents whether the task-list boxes of a rendering can be clicked.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Boxes {
    /// Every box is disabled, as in a comment or for a viewer who cannot edit.
    Disabled,
    /// Every box is enabled, for a viewer who may tick it.
    Tickable,
}

/// Returns the parser options both [`render`] and [`toggle`] read the source with.
fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// Renders `source` as sanitized HTML with disabled task-list boxes, safe to emit without further
/// escaping.
pub fn render(source: &str) -> String {
    render_with(source, Boxes::Disabled)
}

/// Renders `source` like [`render`], with the task-list boxes enabled so a click can tick them.
pub fn render_tickable(source: &str) -> String {
    render_with(source, Boxes::Tickable)
}

/// Returns the lowercase hex SHA-256 of `source`, which names one revision of a description.
pub fn hash(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}

/// Returns `source` with its `index`-th task-list box set to `checked`, or [`None`] when it has
/// fewer boxes.
///
/// Boxes are found by their source offset in the same parse [`render`] numbers them by, so text
/// that only looks like a box, in a code block for instance, never counts. Only the one byte
/// between the brackets changes.
pub fn toggle(source: &str, index: usize, checked: bool) -> Option<String> {
    let (_, range) = Parser::new_ext(source, options())
        .into_offset_iter()
        .filter(|(event, _)| matches!(event, Event::TaskListMarker(_)))
        .nth(index)?;
    let open = range.start + source.get(range.clone())?.find('[')?;
    let mark = open + 1;
    if !matches!(source.as_bytes().get(mark)?, b' ' | b'x' | b'X') {
        return None;
    }
    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..mark]);
    out.push(if checked { 'x' } else { ' ' });
    out.push_str(&source[mark + 1..]);
    Some(out)
}

/// Task-list boxes are marked with a random token drawn for this call, so source text cannot
/// produce it. The marker survives sanitizing and becomes a checkbox afterwards, numbered by its
/// place among the boxes. Images become links to their source.
fn render_with(source: &str, boxes: Boxes) -> String {
    let token = format!("cf{:032x}", rand::thread_rng().r#gen::<u128>());
    let mut next = 0usize;
    let events = Parser::new_ext(source, options()).map(|event| match event {
        Event::TaskListMarker(checked) => {
            let state = if checked { 'x' } else { 'o' };
            let marker = format!("{token}{state}{next}_");
            next += 1;
            Event::Text(marker.into())
        }
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }),
        Event::End(TagEnd::Image) => Event::End(TagEnd::Link),
        other => other,
    });
    let mut unsafe_html = String::new();
    html::push_html(&mut unsafe_html, events);
    let clean = SANITIZER.clean(&unsafe_html).to_string();
    put_boxes(&clean, &token, boxes)
}

/// Replaces each marker `token` left in `clean` with its checkbox.
fn put_boxes(clean: &str, token: &str, boxes: Boxes) -> String {
    let disabled = match boxes {
        Boxes::Disabled => " disabled",
        Boxes::Tickable => "",
    };
    let mut out = String::with_capacity(clean.len());
    let mut rest = clean;
    while let Some(at) = rest.find(token) {
        out.push_str(&rest[..at]);
        let tail = &rest[at + token.len()..];
        let parsed = tail.split_once('_').and_then(|(spec, after)| {
            let (state, index) = spec.split_at_checked(1)?;
            let index = index.parse::<usize>().ok()?;
            Some((state == "x", index, after))
        });
        let Some((checked, index, after)) = parsed else {
            rest = tail;
            continue;
        };
        let checked = if checked { " checked" } else { "" };
        let _ = write!(
            out,
            r#"<input type="checkbox"{disabled}{checked} data-checklist-index="{index}"> "#
        );
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_should_strip_script_handlers_and_javascript_links() {
        let out = render(
            "<script>alert(1)</script>\n\n<img src=x onerror=\"alert(2)\">\n\n[x](javascript:alert(3))\n\n<a href=\"javascript:alert(4)\" onclick=\"z()\">y</a>",
        );
        assert!(!out.contains("<script"), "{out}");
        assert!(!out.contains("onerror"), "{out}");
        assert!(!out.contains("onclick"), "{out}");
        assert!(!out.contains("javascript:"), "{out}");
    }

    #[test]
    fn render_should_open_links_with_noopener_noreferrer() {
        let out = render("[a](https://example.com)");
        assert!(out.contains(r#"href="https://example.com""#), "{out}");
        assert!(out.contains(r#"rel="noopener noreferrer""#), "{out}");
        assert!(out.contains(r#"target="_blank""#), "{out}");
    }

    #[test]
    fn render_should_force_blank_target_over_a_raw_target() {
        let out = render(r#"<a href="https://example.com" target="_self">x</a>"#);
        assert!(out.contains(r#"target="_blank""#), "{out}");
        assert!(!out.contains("_self"), "{out}");
        assert!(out.contains(r#"rel="noopener noreferrer""#), "{out}");
    }

    #[test]
    fn render_should_show_tables_code_blocks_and_task_lists() {
        let out = render(
            "|a|b|\n|-|-|\n|1|2|\n\n```rust\nlet x = 1;\n```\n\n- [x] done\n- [ ] todo\n\n~~gone~~",
        );
        assert!(
            out.contains("<table>") && out.contains("<td>1</td>"),
            "{out}"
        );
        assert!(
            out.contains("<pre><code") && out.contains("let x = 1;"),
            "{out}"
        );
        assert!(out.contains("<del>gone</del>"), "{out}");
    }

    #[test]
    fn render_should_disable_task_list_boxes() {
        let out = render("- [x] done\n- [ ] todo");
        assert!(
            out.contains(
                r#"<input type="checkbox" disabled checked data-checklist-index="0"> done"#
            ),
            "{out}"
        );
        assert!(
            out.contains(r#"<input type="checkbox" disabled data-checklist-index="1"> todo"#),
            "{out}"
        );
    }

    #[test]
    fn render_should_number_the_boxes() {
        let out = render("- [x] a\n  - [ ] b\n- [ ] c");
        for (index, state) in [(0, " checked"), (1, ""), (2, "")] {
            let tag = format!(
                r#"<input type="checkbox" disabled{state} data-checklist-index="{index}">"#
            );
            assert!(out.contains(&tag), "{tag} in {out}");
        }
    }

    #[test]
    fn render_tickable_should_leave_the_boxes_enabled() {
        let out = render_tickable("- [ ] a");
        assert!(
            out.contains(r#"<input type="checkbox" data-checklist-index="0"> a"#),
            "{out}"
        );
        assert!(!out.contains("disabled"), "{out}");
    }

    #[test]
    fn render_should_not_number_boxes_in_code() {
        let out = render("```\n- [ ] not a box\n```\n\n    - [ ] nor this\n\n- [ ] real");
        assert_eq!(out.matches("<input").count(), 1, "{out}");
        assert!(out.contains(r#"data-checklist-index="0""#), "{out}");
    }

    #[test]
    fn render_should_not_let_source_forge_an_index() {
        let out = render(r#"<input type="checkbox" data-checklist-index="3">"#);
        assert!(!out.contains("<input"), "{out}");
        assert!(!out.contains("data-checklist-index"), "{out}");
    }

    #[test]
    fn hash_should_be_the_hex_sha256() {
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn toggle_should_rewrite_only_the_marker_byte() {
        let source = "# T\n\n- [ ] one\n- [x] two\n";
        assert_eq!(
            toggle(source, 0, true).as_deref(),
            Some("# T\n\n- [x] one\n- [x] two\n")
        );
        assert_eq!(
            toggle(source, 1, false).as_deref(),
            Some("# T\n\n- [ ] one\n- [ ] two\n")
        );
    }

    #[test]
    fn toggle_should_skip_markers_in_fences_and_indented_code() {
        let source = "```\n- [ ] fenced\n```\n\n    - [ ] indented\n\n- [ ] real\n";
        assert_eq!(
            toggle(source, 0, true).as_deref(),
            Some("```\n- [ ] fenced\n```\n\n    - [ ] indented\n\n- [x] real\n")
        );
        assert_eq!(toggle(source, 1, true), None);
    }

    #[test]
    fn toggle_should_count_nested_boxes_in_document_order() {
        let source = "- [ ] a\n  - [ ] b\n    - [x] c\n- [ ] d\n";
        assert_eq!(
            toggle(source, 2, false).as_deref(),
            Some("- [ ] a\n  - [ ] b\n    - [ ] c\n- [ ] d\n")
        );
        assert_eq!(
            toggle(source, 3, true).as_deref(),
            Some("- [ ] a\n  - [ ] b\n    - [x] c\n- [x] d\n")
        );
    }

    #[test]
    fn toggle_should_keep_crlf_line_endings() {
        let source = "- [ ] a\r\n- [ ] b\r\n";
        assert_eq!(
            toggle(source, 1, true).as_deref(),
            Some("- [ ] a\r\n- [x] b\r\n")
        );
    }

    #[test]
    fn toggle_should_read_an_uppercase_x_and_write_a_lowercase_one() {
        assert_eq!(toggle("- [X] a", 0, false).as_deref(), Some("- [ ] a"));
        assert_eq!(toggle("- [X] a", 0, true).as_deref(), Some("- [x] a"));
    }

    #[test]
    fn toggle_should_find_a_box_after_multibyte_text() {
        let source = "héllo wörld ✓\n\n- [ ] ünï\n- [ ] 日本語\n";
        assert_eq!(
            toggle(source, 1, true).as_deref(),
            Some("héllo wörld ✓\n\n- [ ] ünï\n- [x] 日本語\n")
        );
    }

    #[test]
    fn toggle_should_refuse_an_index_past_the_last_box() {
        assert_eq!(toggle("- [ ] a", 1, true), None);
        assert_eq!(toggle("no boxes", 0, true), None);
    }

    #[test]
    fn toggle_should_ignore_a_box_that_only_looks_like_one() {
        let source = "[ ] bare\n\n- [ ]no space\n\n- [ ] real\n";
        assert_eq!(
            toggle(source, 0, true).as_deref(),
            Some("[ ] bare\n\n- [ ]no space\n\n- [x] real\n")
        );
    }

    #[test]
    fn render_should_neutralise_raw_inputs() {
        let out = render(
            "<input type=password name=p>\n\n<input type=checkbox checked>\n\n<input type=text value=x>",
        );
        assert!(!out.contains("<input"), "{out}");
        assert!(!out.contains("password"), "{out}");
    }

    #[test]
    fn render_should_not_let_source_forge_a_checkbox() {
        for source in [
            "\u{E000}x forged",
            "&#xE000;x forged",
            "&#xE000;o forged",
            "[a](https://e.com \"&#xE000;o\")",
            "<a href=\"https://e.com\" title=\"&#xE000;x\">a</a>",
        ] {
            let out = render(source);
            assert!(!out.contains("<input"), "{out}");
            if out.contains("<a ") {
                assert!(out.contains(r#"rel="noopener noreferrer""#), "{out}");
            }
        }
    }

    #[test]
    fn render_should_strip_dangerous_urls_and_elements() {
        let out = render(
            "[d](data:text/html,x) [v](vbscript:x)\n\n<p style=\"position:fixed\">s</p>\n\n<iframe src=\"https://e.com\"></iframe>\n\n<svg onload=\"x()\"><circle/></svg>\n\n<base href=\"https://e.com\">\n\n<meta http-equiv=\"refresh\" content=\"0\">",
        );
        for bad in [
            "data:",
            "vbscript:",
            "style",
            "iframe",
            "svg",
            "<base",
            "<meta",
            "onload",
        ] {
            assert!(!out.contains(bad), "{bad} in {out}");
        }
    }

    #[test]
    fn render_should_turn_images_into_links_and_drop_raw_img() {
        let out = render("![alt text](https://e.com/p.png)\n\n<img src=\"https://e.com/q.png\">");
        assert!(!out.contains("<img"), "{out}");
        assert!(
            out.contains(r#"<a href="https://e.com/p.png""#) && out.contains("alt text"),
            "{out}"
        );
    }

    #[test]
    fn render_should_escape_plain_text() {
        assert_eq!(render("a < b & c"), "<p>a &lt; b &amp; c</p>\n");
    }
}
