use std::collections::HashSet;
use std::sync::LazyLock;

use ammonia::Builder;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};
use rand::Rng;

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

/// Renders `source` as sanitized HTML, safe to emit without further escaping.
///
/// Task-list boxes are marked with a random token drawn for this call, so source text cannot
/// produce it. The marker survives sanitizing and becomes a disabled checkbox afterwards.
/// Images become links to their source.
pub fn render(source: &str) -> String {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let token = format!("cf{:032x}", rand::thread_rng().r#gen::<u128>());
    let checked = format!("{token}x");
    let unchecked = format!("{token}o");
    let events = Parser::new_ext(source, options).map(|event| match event {
        Event::TaskListMarker(true) => Event::Text(checked.clone().into()),
        Event::TaskListMarker(false) => Event::Text(unchecked.clone().into()),
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
    SANITIZER
        .clean(&unsafe_html)
        .to_string()
        .replace(&checked, r#"<input type="checkbox" disabled checked> "#)
        .replace(&unchecked, r#"<input type="checkbox" disabled> "#)
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
            out.contains(r#"<input type="checkbox" disabled checked> done"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<input type="checkbox" disabled> todo"#),
            "{out}"
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
