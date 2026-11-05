//! The Markdown boundary against the pinned parser itself: real views in a real (test) window,
//! the extensions the panel builds its views with, and a tripwire behind them.

use super::*;
use gpui::{Context, Entity, Render, TestAppContext, VisualTestContext};
use gpui_component::text::TextViewState;
use std::sync::{Arc, Mutex};

/// Stands behind the plugins under test and records every image and every piece of HTML the view
/// offers it, which is exactly what they did not claim: the view builds its own element (and may
/// fetch) for each of those. It claims nothing itself.
struct Tripwire {
    block: bool,
    offered: Arc<Mutex<Vec<String>>>,
}

impl MarkdownPlugin for Tripwire {
    fn is_block(&self) -> bool {
        self.block
    }

    fn name(&self) -> &str {
        if self.block {
            "tripwire-block"
        } else {
            "tripwire-inline"
        }
    }

    fn parse(&self, node: &Node, _: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        if matches!(
            node,
            Node::Image(_) | Node::ImageReference(_) | Node::Html(_)
        ) {
            self.offered
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(format!("{node:?}"));
        }
        None
    }
}

/// The extensions of the panel, then the tripwire.
fn watched(offered: &Arc<Mutex<Vec<String>>>) -> MarkdownExtensions {
    [false, true]
        .into_iter()
        .fold(extensions(), |extensions, block| {
            extensions.plugin(Tripwire {
                block,
                offered: Arc::clone(offered),
            })
        })
}

/// Markdown views, one per text, in one window.
struct Host {
    texts: Vec<SharedString>,
    extensions: MarkdownExtensions,
}

impl Render for Host {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().children(
            self.texts
                .iter()
                .enumerate()
                .map(|(ix, text)| view(("probe", ix), text.clone(), &self.extensions)),
        )
    }
}

struct Rig {
    host: Entity<Host>,
    states: Arc<Mutex<Vec<Entity<TextViewState>>>>,
    offered: Arc<Mutex<Vec<String>>>,
}

fn rig(
    cx: &mut TestAppContext,
    extensions: impl FnOnce(&Arc<Mutex<Vec<String>>>) -> MarkdownExtensions,
) -> (Rig, &mut VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::theme::init(cx);
    });
    let states = Arc::new(Mutex::new(Vec::new()));
    cx.update(|cx| {
        let states = Arc::clone(&states);
        cx.observe_new(move |_: &mut TextViewState, _, cx| {
            states.lock().unwrap().push(cx.entity());
        })
        .detach();
    });
    let offered = Arc::new(Mutex::new(Vec::new()));
    let extensions = extensions(&offered);
    let (host, visual) = cx.add_window_view(|_, _| Host {
        texts: Vec::new(),
        extensions,
    });
    (
        Rig {
            host,
            states,
            offered,
        },
        visual,
    )
}

impl Rig {
    fn settle(&self, visual: &mut VisualTestContext) {
        for _ in 0..4 {
            visual.update(|window, _| window.refresh());
            visual.run_until_parked();
        }
    }

    fn show(&self, texts: &[&str], visual: &mut VisualTestContext) {
        self.host.update(visual, |host, cx| {
            host.texts = texts
                .iter()
                .map(|text| SharedString::from(text.to_string()))
                .collect();
            cx.notify();
        });
        self.settle(visual);
    }

    /// What the view of `text` renders (the string plain copy produces).
    fn rendered(&self, text: &str, visual: &mut VisualTestContext) -> String {
        let states = self.states.lock().unwrap().clone();
        states
            .iter()
            .find_map(|state| {
                state.read_with(visual, |state, _| {
                    let rendered = state.rendered_text();
                    (rendered.source() == text).then(|| rendered.as_str().to_owned())
                })
            })
            .unwrap_or_else(|| panic!("no view parsed {text:?}"))
    }

    fn offered(&self) -> Vec<String> {
        self.offered.lock().unwrap().clone()
    }
}

/// Text that is not code and holds an image or HTML in every position the parser knows, and the
/// source each must come out as.
const PROSE_CASES: &[(&str, &str, &str)] = &[
    (
        "a code span across lines",
        "a ` b\nc ` ![x](https://h/?d=rows) ` d",
        "![x](https://h/?d=rows)",
    ),
    (
        "a math span that swallows a backtick",
        "$`$ ![x](https://h/p.png) `",
        "![x](https://h/p.png)",
    ),
    (
        "a fence closed by its list item",
        "- x\n  ```\n![x](https://h/p.png)\n",
        "![x](https://h/p.png)",
    ),
    (
        "an indented fence under a list item, then prose",
        "- item\n\n      ```sql\n      SELECT a < b;\n      ```\n\n  ![x](https://h/p.png)\n",
        "![x](https://h/p.png)",
    ),
    (
        "a data image",
        "see ![x](data:image/png;base64,iVBORw0KGgo=) here",
        "![x](data:image/png;base64,iVBORw0KGgo=)",
    ),
    (
        "a file image",
        "![x](file:///etc/passwd)",
        "![x](file:///etc/passwd)",
    ),
    (
        "an image by reference with its definition",
        "![a][b]\n\n[b]: https://h/?d=rows\n",
        "![a][b]",
    ),
    (
        "a collapsed and a shortcut reference",
        "![a][] and ![a]\n\n[a]: https://h/p.png\n",
        "![a][]",
    ),
    (
        "an image inside link text",
        "[![x](https://h/p.png)](https://example.com/)",
        "![x](https://h/p.png)",
    ),
    (
        "an image inside emphasis and strong",
        "*![x](https://h/p.png)* and **![y](https://h/q.png)**",
        "![y](https://h/q.png)",
    ),
    (
        "an image in a table cell",
        "| a | b |\n|---|---|\n| ![x](https://h/p.png) | <img src=y> |\n",
        "![x](https://h/p.png)",
    ),
    (
        "an image in a blockquote and in a list",
        "> ![x](https://h/p.png)\n\n- ![y](https://h/q.png)\n  - ![z](https://h/r.png)\n",
        "![z](https://h/r.png)",
    ),
    (
        "an image in a heading",
        "# ![x](https://h/p.png) title\n",
        "![x](https://h/p.png)",
    ),
    (
        "an image in a footnote",
        "text[^1]\n\n[^1]: ![x](https://h/p.png)\n",
        "![x](https://h/p.png)",
    ),
    (
        "an inline tag",
        "see <img src=\"https://h/q\"> end",
        "<img src=\"https://h/q\">",
    ),
    (
        "an HTML block",
        "<div>\n![x](https://h/p.png)\n</div>\n",
        "<div>",
    ),
    (
        "an HTML block with a blank line",
        "<pre>\nline\n\n<img src=x>\n</pre>\n",
        "<img src=x>",
    ),
    (
        "a comment",
        "before <!-- ![x](https://h/p.png) --> after\n",
        "<!-- ![x](https://h/p.png) -->",
    ),
    ("an unpaired formatting tag", "x <b>bold <img src=y>", "<b>"),
    (
        "an image whose text is a multi-line title",
        "![x](https://h/p.png \"a\nb\")",
        "![x](https://h/p.png",
    ),
];

#[gpui::test]
fn no_image_and_no_html_is_built_from_text_outside_code(cx: &mut TestAppContext) {
    let (rig, visual) = rig(cx, watched);
    let texts: Vec<&str> = PROSE_CASES.iter().map(|(_, text, _)| *text).collect();
    rig.show(&texts, visual);
    for (name, text, needle) in PROSE_CASES {
        let rendered = rig.rendered(text, visual);
        assert!(
            rendered.contains(needle),
            "{name}: the source of the node must show as text\nwanted {needle:?}\nrendered {rendered:?}"
        );
    }
    assert_eq!(
        rig.offered(),
        Vec::<String>::new(),
        "an image or HTML node reached the view's own treatment"
    );
}

/// The tripwire works: without the plugins it sees the images and the HTML (so an empty list above
/// means something).
#[gpui::test]
fn control_the_tripwire_sees_what_the_view_would_build_without_the_plugins(
    cx: &mut TestAppContext,
) {
    let (rig, visual) = rig(cx, |offered| {
        [false, true]
            .into_iter()
            .fold(MarkdownExtensions::default(), |extensions, block| {
                extensions.plugin(Tripwire {
                    block,
                    offered: Arc::clone(offered),
                })
            })
    });
    let texts: Vec<&str> = PROSE_CASES.iter().map(|(_, text, _)| *text).collect();
    rig.show(&texts, visual);
    let offered = rig.offered().join("\n");
    for kind in ["Image {", "ImageReference {", "Html {"] {
        assert!(
            offered.contains(kind),
            "{kind} was never offered: {offered}"
        );
    }
}

/// Code and math are what the model wrote, byte for byte, and prose keeps its `<`.
#[gpui::test]
fn code_and_math_are_left_exactly_as_written_and_prose_renders_as_typed(cx: &mut TestAppContext) {
    let (rig, visual) = rig(cx, watched);
    let cases: &[(&str, &str, &str)] = &[
        (
            "a fenced block",
            "```sql\nSELECT a < b, '![x](y)', '$1' FROM t;\n```\n",
            "SELECT a < b, '![x](y)', '$1' FROM t;",
        ),
        (
            "a fence under a list item, four spaces in",
            "- item\n\n      ```sql\n      SELECT a < b, '![x](y)';\n      ```\n",
            "SELECT a < b, '![x](y)';",
        ),
        (
            "a fence on the marker line",
            "- ```sql\n  SELECT a < b, '![x](y)';\n  ```\n",
            "SELECT a < b, '![x](y)';",
        ),
        (
            "a fence in a blockquote",
            "> ```sql\n> SELECT a < b, '![x](y)';\n> ```\n",
            "SELECT a < b, '![x](y)';",
        ),
        (
            "an indented code block",
            "Run:\n\n    SELECT a < b, '![x](y)';\n",
            "SELECT a < b, '![x](y)';",
        ),
        ("a math block", "$$\na < b ![x](y)\n$$\n", "a < b ![x](y)"),
        (
            "a code span",
            "use `a < b ![x](y)` here",
            "use a < b ![x](y) here",
        ),
        (
            "prose with a comparison",
            "when a < b and 5 > 3 then c<d",
            "when a < b and 5 > 3 then c<d",
        ),
        (
            "an entity",
            "&#60;img src=x&#62; and &lt;b&gt;",
            "<img src=x> and <b>",
        ),
    ];
    let texts: Vec<&str> = cases.iter().map(|(_, text, _)| *text).collect();
    rig.show(&texts, visual);
    for (name, text, wanted) in cases {
        let rendered = rig.rendered(text, visual);
        assert!(
            rendered.contains(wanted),
            "{name}\nwanted {wanted:?}\nrendered {rendered:?}"
        );
        assert!(
            !rendered.contains('\\'),
            "{name}: an escape shows: {rendered:?}"
        );
    }
    assert_eq!(rig.offered(), Vec::<String>::new());
}

/// A streamed answer is parsed again as it grows, a block at a time: no prefix of it, cut anywhere,
/// builds an image or HTML.
#[gpui::test]
fn a_streamed_answer_never_builds_an_image_at_any_prefix(cx: &mut TestAppContext) {
    let (rig, visual) = rig(cx, watched);
    let full = "text ![a](https://h/?q=1) <img src=x> more [l](https://h/) ![r][k] `![c](d)` \
                <!-- ![e](f) -->\n\n```\n![g](h)\n```\n\n[k]: https://h/z\n";
    for end in (0..=full.len()).filter(|end| full.is_char_boundary(*end)) {
        rig.show(&[&full[..end]], visual);
        assert_eq!(
            rig.offered(),
            Vec::<String>::new(),
            "an image or HTML was built from the prefix {:?}",
            &full[..end]
        );
    }
    let rendered = rig.rendered(full, visual);
    assert!(rendered.contains("![a](https://h/?q=1)"), "{rendered:?}");
    assert!(rendered.contains("<img src=x>"), "{rendered:?}");
}

#[test]
fn a_long_source_is_clipped_to_a_line_that_can_be_scaled() {
    let data = format!("![x](data:image/png;base64,{})", "A".repeat(100_000));
    let shown = clip(&data, INLINE_SHOWN_CHARS);
    assert_eq!(shown.chars().count(), INLINE_SHOWN_CHARS + 1);
    assert!(shown.starts_with("![x](data:image/png;base64,AAAA") && shown.ends_with('\u{2026}'));
    assert_eq!(clip("short", 160), "short");
    // Characters, not bytes: no cut inside one.
    assert_eq!(clip("ééé", 2), "éé\u{2026}");
}

#[test]
fn user_text_is_escaped_so_it_renders_as_typed() {
    assert_eq!(escape_plain("SELECT * FROM t_1"), "SELECT \\* FROM t\\_1");
    assert_eq!(escape_plain("a\nb"), "a\\\nb");
    assert_eq!(escape_plain("# not a heading"), "\\# not a heading");
    assert_eq!(
        escape_plain("    indented"),
        "\u{00A0}\u{00A0}\u{00A0}\u{00A0}indented"
    );
    assert_eq!(escape_plain("1. one"), "1\\. one");
    assert_eq!(escape_plain("é ü"), "é ü");
}

// ---- where a link goes ----------------------------------------------------------------------------

fn opens(raw: &str) -> (String, Option<String>, Option<String>) {
    match destination(raw) {
        Destination::Open {
            url,
            site,
            userinfo,
        } => (url, site, userinfo),
        other => panic!("{raw:?} should open, got {other:?}"),
    }
}

fn refused(raw: &str) -> String {
    match destination(raw) {
        Destination::Refused { shown } => shown,
        other => panic!("{raw:?} should be refused, got {other:?}"),
    }
}

#[test]
fn a_web_address_opens_as_its_normalised_form_with_the_site_apart() {
    let (url, site, userinfo) = opens("https://example.com/docs?a=1#top");
    assert_eq!(url, "https://example.com/docs?a=1#top");
    assert_eq!((site.as_deref(), userinfo), (Some("example.com"), None));
    // Case, spaces and non-ASCII are normalised: what shows is what opens.
    let (url, site, _) = opens("HTTP://Example.COM/a b/é");
    assert_eq!(url, "http://example.com/a%20b/%C3%A9");
    assert_eq!(site.as_deref(), Some("example.com"));
    // Mail has no site.
    let (url, site, userinfo) = opens("mailto:someone@example.com?subject=hi");
    assert_eq!(url, "mailto:someone@example.com?subject=hi");
    assert_eq!((site, userinfo), (None, None));
}

#[test]
fn a_name_before_the_at_sign_is_called_out_and_the_site_is_the_real_one() {
    let (url, site, userinfo) = opens("https://good.example@evil.example/login");
    assert_eq!(site.as_deref(), Some("evil.example"));
    assert_eq!(userinfo.as_deref(), Some("good.example"));
    assert_eq!(url, "https://good.example@evil.example/login");
    let (_, site, userinfo) = opens("https://user:secret@evil.example/");
    assert_eq!(site.as_deref(), Some("evil.example"));
    assert_eq!(userinfo.as_deref(), Some("user"));
}

#[test]
fn look_alike_and_reordering_characters_do_not_reach_the_dialog_or_the_browser() {
    // A look-alike letter in the host shows in its ASCII (punycode) form.
    let (url, site, _) = opens("https://\u{0430}pple.com/");
    assert_eq!(site.as_deref(), Some("xn--pple-43d.com"));
    assert!(url.is_ascii(), "{url}");
    // Reordering and zero-width characters are removed or encoded, never shown.
    for raw in [
        "https://example.com/\u{202E}txt.exe",
        "https://example.com/a\u{200B}b\u{2066}c",
        "https://example.com/\u{FEFF}x\u{061C}",
        "https://example.com/\u{0007}\u{0000}x",
    ] {
        match destination(raw) {
            Destination::Open { url, .. } => {
                assert!(url.is_ascii(), "{raw:?} -> {url}");
                assert_eq!(displayable(&url), url, "nothing is dropped from what opens");
            }
            Destination::Refused { shown } => {
                assert!(!shown.chars().any(is_invisible), "{raw:?} -> {shown:?}")
            }
        }
    }
    // A host with a reordering character does not parse at all.
    let shown = refused("https://exa\u{202E}mple.com/");
    assert!(!shown.chars().any(is_invisible), "{shown:?}");
}

#[test]
fn what_is_not_a_web_or_mail_address_is_refused_and_shown_clean() {
    for raw in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "data:text/html;base64,PHNjcmlwdD4=",
        "ftp://example.com/x",
        "tel:+123",
        "/relative/path",
        "not a url",
        "",
        "https://",
        "http://[::1",
    ] {
        let shown = refused(raw);
        assert!(!shown.chars().any(is_invisible), "{raw:?} -> {shown:?}");
    }
    let shown = refused("file:///etc/pass\u{202E}wd\u{0000}");
    assert_eq!(shown, "file:///etc/passwd");
    let long = refused(&format!("data:{}", "A".repeat(10_000)));
    assert_eq!(long.chars().count(), SHOWN_ADDRESS_CHARS + 1);
}

#[test]
fn an_address_longer_than_the_confirmation_shows_is_not_opened() {
    // The tail of a long address would open unseen: a query that spells out rows, a `bcc`.
    let padding = "a".repeat(SHOWN_ADDRESS_CHARS);
    for raw in [
        format!("https://docs.example.com/{padding}?rows=secret"),
        format!("mailto:a@b.example?subject={padding}&bcc=x@evil.example"),
    ] {
        let shown = refused(&raw);
        assert_eq!(shown.chars().count(), SHOWN_ADDRESS_CHARS + 1, "{raw}");
    }
    // One that fits is shown whole, and that is what opens.
    let fits = format!(
        "https://docs.example.com/{}",
        "a".repeat(SHOWN_ADDRESS_CHARS - 40)
    );
    match destination(&fits) {
        Destination::Open { url, .. } => assert_eq!(displayable(&url), url),
        other => panic!("{other:?}"),
    }
}
