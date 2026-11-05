//! What reaches the Markdown view, and where a link in it goes.
//!
//! * [`extensions`]: a model's answer may carry text from the data it read, so what a Markdown view
//!   would *do* on its own must not happen: an image would be fetched (a URL that spells out rows
//!   read is a zero-click leak the day the app gives GPUI an HTTP client), raw HTML would be
//!   rendered. The decision is the parser's, not a scan of the text's: the pinned view offers every
//!   inline node and every block node to the plugins of [`MarkdownExtensions`] before it builds
//!   anything, and these plugins claim images (inline and by reference) and raw HTML (inline and
//!   block) and show their *source* as plain text instead. Code (fences, indented blocks, spans)
//!   and math are other node kinds the plugins never see, so they stay byte for byte what the
//!   model wrote, which is what Insert and Copy hand out. Links stay links, and a click asks
//!   before it opens anything ([`destination`], and the panel).
//! * [`escape_plain`]: the user's own message is text, not Markdown (`*`, `_`, `#` in a question
//!   are what they typed). The pinned `TextView` has constructors for Markdown and HTML only, so
//!   the text is escaped for Markdown instead: ASCII punctuation gets a backslash (CommonMark
//!   renders `\*` as `*`), line ends become hard breaks, and leading blanks (which would make an
//!   indented code block) become non-breaking spaces.
//!
//! One thing the plugins cannot see: the pinned view pairs the formatting tags `<b>`, `<strong>`,
//! `<i>`, `<em>`, `<u>`, `<s>`, `<del>` and `<strike>` among the siblings of a paragraph before any
//! plugin is asked, and turns a pair into bold, italic, underline or strike-through on the text
//! between. No HTML node and no element is built from them; only the text's style changes.

use gpui::{
    div, px, App, ElementId, IntoElement, ParentElement as _, Pixels, SharedString,
    StyleRefinement, Styled as _, Window,
};
use gpui_component::text::markdown_ast::Node;
use gpui_component::text::{
    InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
    MarkdownPlugin, TextView, TextViewStyle,
};

/// The room above a code block's first line for the buttons in its corner.
const CODE_TOP: Pixels = px(30.0);

/// The longest source of an image or an inline tag that is shown: it is one unbreakable item of a
/// line (the view scales it to fit the width), and a `data:` URL can be megabytes.
const INLINE_SHOWN_CHARS: usize = 160;

/// What the answers' Markdown views are built with.
pub fn extensions() -> MarkdownExtensions {
    MarkdownExtensions::default()
        .plugin(Source { block: false })
        .plugin(Source { block: true })
}

/// A selectable Markdown view of `text` that fetches and renders nothing by itself.
pub fn view(
    id: impl Into<ElementId>,
    text: SharedString,
    extensions: &MarkdownExtensions,
) -> TextView {
    TextView::markdown(id, text)
        .selectable(true)
        // The Copy / Insert buttons sit in the block's top right corner: the code starts under
        // them, so they never cover its first line.
        .style(TextViewStyle::default().code_block(StyleRefinement::default().pt(CODE_TOP)))
        .markdown_extensions(extensions.clone())
}

/// Claims what a Markdown view would load or render, and shows its source as text. Registered
/// twice: the inline form sees images, image references and inline tags; the block form sees
/// HTML blocks.
struct Source {
    block: bool,
}

impl MarkdownPlugin for Source {
    fn is_block(&self) -> bool {
        self.block
    }

    fn name(&self) -> &str {
        if self.block {
            "source-block"
        } else {
            "source-inline"
        }
    }

    fn parse(&self, node: &Node, cx: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let claimed = match node {
            Node::Html(_) => true,
            Node::Image(_) | Node::ImageReference(_) => !self.block,
            _ => false,
        };
        if !claimed {
            return None;
        }
        let source = cx.node_source(node).unwrap_or_default();
        let shown = if self.block {
            source.to_owned()
        } else {
            clip(source, INLINE_SHOWN_CHARS)
        };
        Some(MarkdownNode::new(self.name().to_owned(), ()).text(shown))
    }

    fn render(&self, node: &MarkdownNode, _: &mut Window, _: &mut App) -> impl IntoElement {
        div().child(SharedString::from(node.as_text().to_owned()))
    }

    /// No element: the plain text of the node, laid out with the line it sits in.
    fn render_inline(
        &self,
        _: &MarkdownNode,
        _: &InlineRenderContext,
        _: &mut Window,
        _: &mut App,
    ) -> Option<InlineElement> {
        None
    }
}

/// `text` cut to `max` characters, with an ellipsis when something went.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}\u{2026}", &text[..end]),
        None => text.to_owned(),
    }
}

/// `text` as Markdown that renders as exactly this text.
pub fn escape_plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            // A hard break: a backslash at the end of the line.
            out.push_str("\\\n");
        }
        let line = line.trim_end_matches('\r');
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        for _ in 0..indent {
            out.push('\u{00A0}');
        }
        for c in line[indent..].chars() {
            if c.is_ascii_punctuation() {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

// ---- links ----------------------------------------------------------------------------------------

/// The longest address the confirmation shows, in characters.
const SHOWN_ADDRESS_CHARS: usize = 300;

/// Where a clicked link goes, as the confirmation shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A web or mail address; it opens after a confirmation, and `url` is what opens.
    Open {
        /// The normalised address: the text the confirmation shows and the one that is opened.
        url: String,
        /// The host of a web address (in its ASCII form, so look-alike letters are visible), shown
        /// on a line of its own. `None` for mail.
        site: Option<String>,
        /// The name before the `@` of a web address (`good` in `https://good@evil.example/`): a
        /// reader takes it for the site.
        userinfo: Option<String>,
    },
    /// Nothing opens; this is what the dialog shows of it.
    Refused { shown: String },
}

/// Characters that change how text is laid out or hide in it, without a shape of their own. One
/// set for the model's view, this panel's links and the approval card: the engine's.
fn is_invisible(c: char) -> bool {
    engine_agent::sanitize::is_shapeless(c)
}

/// `raw` as it is safe to show: no invisible character, and not endless.
pub fn displayable(raw: &str) -> String {
    let visible: String = raw.chars().filter(|c| !is_invisible(*c)).collect();
    clip(&visible, SHOWN_ADDRESS_CHARS)
}

/// Parses the target of a link. Only web and mail addresses can be opened, and what opens is the
/// parsed, normalised address (percent-encoded, the host in its ASCII form), which holds no
/// invisible character at all: what the confirmation shows is what opens. An address longer than
/// the confirmation shows is refused for that reason: its tail (a query that spells out data, a
/// `bcc` of a mail link) would open unseen.
pub fn destination(raw: &str) -> Destination {
    let refused = || Destination::Refused {
        shown: displayable(raw),
    };
    let Ok(url) = url::Url::parse(raw) else {
        return refused();
    };
    if url.as_str().chars().count() > SHOWN_ADDRESS_CHARS {
        return refused();
    }
    match url.scheme() {
        "http" | "https" => {
            let Some(host) = url.host_str().filter(|host| !host.is_empty()) else {
                return refused();
            };
            let userinfo = (!url.username().is_empty() || url.password().is_some())
                .then(|| url.username().to_owned());
            Destination::Open {
                site: Some(host.to_owned()),
                userinfo,
                url: url.into(),
            }
        }
        "mailto" => Destination::Open {
            url: url.into(),
            site: None,
            userinfo: None,
        },
        _ => refused(),
    }
}

#[cfg(test)]
mod tests;
