//! Diagram export: SVG (drawn from the layout, not a screenshot), PNG
//! (that SVG rasterized), Mermaid `erDiagram`, DBML and a JSON snapshot. Pure
//! and blocking (no GPUI): the caller runs it on the background executor.
//!
//! Theme: exports always use a fixed LIGHT palette, whatever theme the app
//! shows. They are meant to be pasted into docs and tickets, where a dark
//! canvas is unreadable, and a fixed palette keeps the output deterministic.
//! SVG / PNG draw what the diagram shows (attribute mode, relationship
//! notation, group frames, notes, entity colors, hidden groups left out);
//! Mermaid, DBML and JSON always list every table and column, with the
//! logical relationships, colors, groups and notes in the forms each format
//! has for them.

use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use serde::Serialize;

use super::annotations::{group_frame, Annotations, Group, GroupIndex, Note, Rgb, TableKey};
use super::layout::{AttributeMode, DiagramLayout, DiagramPoint, DiagramRect, Metrics, RoutedRelation};
use super::model::{
    DiagramColumn, DiagramEntity, DiagramGraph, DiagramRelation, EntityState, RelationOrigin, TypeGlyph,
};
use super::notation::{route_directions, EndMark, MarkShape, RelationNotation};

/// A PNG is rendered at this scale unless it would exceed the caps below.
pub(in crate::app) const PNG_SCALE: f32 = 2.0;
/// Longest PNG side in pixels.
pub(in crate::app) const PNG_MAX_SIDE: f32 = 16384.0;
/// Most PNG pixels. The longest side alone would allow 16384 squared (1 GiB of
/// RGBA); this bounds the pixmap at 256 MiB instead.
pub(in crate::app) const PNG_MAX_PIXELS: f32 = 64.0 * 1024.0 * 1024.0;
/// Margin around the drawing, diagram units.
const PADDING: f32 = 24.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum ExportFormat {
    Svg,
    Png,
    Mermaid,
    Dbml,
    Json,
}

impl ExportFormat {
    pub(in crate::app) const ALL: [Self; 5] = [Self::Svg, Self::Png, Self::Mermaid, Self::Dbml, Self::Json];

    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::Svg => "SVG image",
            Self::Png => "PNG image",
            Self::Mermaid => "Mermaid (erDiagram)",
            Self::Dbml => "DBML",
            Self::Json => "JSON snapshot",
        }
    }

    pub(in crate::app) fn extension(self) -> &'static str {
        match self {
            Self::Svg => "svg",
            Self::Png => "png",
            Self::Mermaid => "mmd",
            Self::Dbml => "dbml",
            Self::Json => "json",
        }
    }

    /// `<stem>.<extension>` with the stem reduced to file-name safe characters.
    pub(in crate::app) fn file_name(self, stem: &str) -> String {
        let stem: String =
            stem.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
        let stem = stem.trim_matches(['-', '.']);
        format!("{}.{}", if stem.is_empty() { "diagram" } else { stem }, self.extension())
    }
}

/// Everything an export needs, owned so it can move to a background thread.
#[derive(Debug, Clone)]
pub(in crate::app) struct ExportRequest {
    pub(in crate::app) graph: Arc<DiagramGraph>,
    pub(in crate::app) layout: DiagramLayout,
    pub(in crate::app) mode: AttributeMode,
    pub(in crate::app) notation: RelationNotation,
    pub(in crate::app) metrics: Metrics,
    /// Logical relationships live in `graph`; notes, colors and groups here.
    pub(in crate::app) annotations: Arc<Annotations>,
}

#[derive(Debug)]
pub(in crate::app) enum ExportError {
    Svg(resvg::usvg::Error),
    /// The pixmap for the requested size could not be allocated.
    Pixmap {
        width: u32,
        height: u32,
    },
    Png(String),
    Json(serde_json::Error),
    Io(std::io::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Svg(error) => write!(f, "could not read the generated SVG: {error}"),
            Self::Pixmap { width, height } => write!(f, "could not allocate a {width}x{height} image"),
            Self::Png(error) => write!(f, "could not encode the PNG: {error}"),
            Self::Json(error) => write!(f, "could not encode the JSON: {error}"),
            Self::Io(error) => write!(f, "could not write the file: {error}"),
        }
    }
}

impl std::error::Error for ExportError {}

impl ExportRequest {
    /// Renders `format` and writes it to `path` (blocking).
    pub(in crate::app) fn write(&self, format: ExportFormat, path: &std::path::Path) -> Result<(), ExportError> {
        std::fs::write(path, self.render(format)?).map_err(ExportError::Io)
    }

    pub(in crate::app) fn render(&self, format: ExportFormat) -> Result<Vec<u8>, ExportError> {
        match format {
            ExportFormat::Svg => Ok(self.svg().into_bytes()),
            ExportFormat::Png => self.png(),
            ExportFormat::Mermaid => Ok(self.mermaid().into_bytes()),
            ExportFormat::Dbml => Ok(self.dbml().into_bytes()),
            ExportFormat::Json => self.json().map(String::into_bytes),
        }
    }
}

// ---- SVG -----------------------------------------------------------------------

/// The fixed light palette.
mod palette {
    pub(super) const BACKGROUND: &str = "#ffffff";
    pub(super) const SURFACE: &str = "#ffffff";
    pub(super) const BORDER: &str = "#c4cad4";
    pub(super) const HEADER: &str = "#eef1f6";
    pub(super) const HEADER_FOCUS: &str = "#dce8ff";
    pub(super) const TEXT: &str = "#1d2330";
    pub(super) const FAINT: &str = "#6b7385";
    pub(super) const LINE: &str = "#6b7385";
    pub(super) const KEY: &str = "#b45309";
    pub(super) const ACCENT: &str = "#2563eb";
    pub(super) const DANGER: &str = "#c62828";
    pub(super) const NOTE_FILL: &str = "#fff6b0";
    pub(super) const NOTE_BORDER: &str = "#d9b92e";
    pub(super) const NOTE_TEXT: &str = "#3d3500";
}

/// Dash pattern of logical relationships.
const LOGICAL_DASH: &str = "7 4";
/// How much of an entity's color tints its header in the (light) export.
const COLOR_TINT: f32 = 0.38;

const FONT: &str = "Helvetica, Arial, sans-serif";

/// Number with at most two decimals, no trailing zeros.
fn num(value: f32) -> String {
    let rounded = (value * 100.0).round() / 100.0;
    format!("{rounded}")
}

fn xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if c.is_control() && c != '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn glyph_text(glyph: TypeGlyph) -> &'static str {
    match glyph {
        TypeGlyph::Numeric => "123",
        TypeGlyph::Text => "A-Z",
        TypeGlyph::Temporal => "dt",
        TypeGlyph::Boolean => "t/f",
        TypeGlyph::Binary => "01",
        TypeGlyph::Json => "{}",
        TypeGlyph::Other => "?",
    }
}

impl ExportRequest {
    /// The color of entity `entity`, if it has one.
    fn color_of(&self, entity: &DiagramEntity) -> Option<Rgb> {
        self.annotations.colors.get(&TableKey::of(&entity.table)).copied()
    }

    /// Frame of group `index` (into `annotations.groups`) unless hidden.
    fn group_frame(&self, index: usize, groups: &GroupIndex) -> Option<DiagramRect> {
        let group = self.annotations.groups.get(index).filter(|g| !g.hidden)?;
        let union = self.layout.union_of(groups.members.get(index)?)?;
        Some(group_frame(union, group.collapsed))
    }

    /// Routes between shown entities.
    fn shown_routes(&self, groups: &GroupIndex) -> Vec<RoutedRelation> {
        let mut routes = self.layout.route(&self.graph, self.mode, &self.metrics);
        routes.retain(|route| {
            self.graph.relations.get(route.relation).is_some_and(|r| groups.shown(r.child.0) && groups.shown(r.parent.0))
        });
        routes
    }

    /// Bounds of boxes, routes, group frames and notes.
    fn extent(&self, routes: &[RoutedRelation], groups: &GroupIndex) -> DiagramRect {
        let boxes = self.layout.rects.iter().enumerate().filter(|(i, _)| groups.shown(*i)).map(|(_, r)| *r);
        let notes = self.annotations.notes.iter().map(Note::rect);
        let frames = (0..self.annotations.groups.len()).filter_map(|g| self.group_frame(g, groups));
        let mut extent = boxes.chain(notes).chain(frames).reduce(|a, b| a.union(&b)).unwrap_or_default();
        let (mut x0, mut y0, mut x1, mut y1) = (extent.x, extent.y, extent.x + extent.width, extent.y + extent.height);
        for p in routes.iter().flat_map(|r| &r.points) {
            (x0, y0, x1, y1) = (x0.min(p.x), y0.min(p.y), x1.max(p.x), y1.max(p.y));
        }
        extent = DiagramRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 };
        extent
    }

    /// The diagram as a standalone SVG document.
    pub(in crate::app) fn svg(&self) -> String {
        let groups = GroupIndex::build(&self.graph, &self.annotations.groups);
        let routes = self.shown_routes(&groups);
        let extent = self.extent(&routes, &groups);
        let width = (extent.width + 2.0 * PADDING).ceil().max(1.0);
        let height = (extent.height + 2.0 * PADDING).ceil().max(1.0);
        let (ox, oy) = (extent.x - PADDING, extent.y - PADDING);
        let mut svg = String::with_capacity(512 + self.graph.entities.len() * 600);
        let _ = writeln!(
            svg,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" font-family=\"{FONT}\">",
            w = num(width),
            h = num(height),
        );
        let _ = writeln!(svg, "<rect width=\"100%\" height=\"100%\" fill=\"{}\"/>", palette::BACKGROUND);
        let _ = writeln!(svg, "<g transform=\"translate({} {})\">", num(-ox), num(-oy));
        for (index, group) in self.annotations.groups.iter().enumerate() {
            if let Some(frame) = self.group_frame(index, &groups) {
                let members = groups.members.get(index).map_or(0, Vec::len);
                group_svg(&mut svg, group, frame, members);
            }
        }
        for routed in &routes {
            self.relation_svg(&mut svg, routed);
        }
        for (index, (entity, rect)) in self.graph.entities.iter().zip(&self.layout.rects).enumerate() {
            if groups.shown(index) {
                self.entity_svg(&mut svg, index, entity, *rect);
            }
        }
        for note in &self.annotations.notes {
            note_svg(&mut svg, note);
        }
        svg.push_str("</g>\n</svg>\n");
        svg
    }

    fn relation_svg(&self, svg: &mut String, routed: &RoutedRelation) {
        let Some(relation) = self.graph.relations.get(routed.relation) else {
            return;
        };
        let points = &routed.points;
        let Some(directions) = route_directions(points) else {
            return;
        };
        let path: String = points
            .iter()
            .enumerate()
            .map(|(i, p)| format!("{}{} {}", if i == 0 { "M" } else { " L" }, num(p.x), num(p.y)))
            .collect();
        let logical = matches!(relation.origin, RelationOrigin::Logical(_));
        let _ = write!(
            svg,
            "<g class=\"relation{}\"><title>{}</title><path d=\"{path}\" fill=\"none\" stroke=\"{}\" stroke-width=\"1.25\"{}/>",
            if logical { " logical" } else { "" },
            xml(&self.relation_title(relation)),
            palette::LINE,
            if logical { format!(" stroke-dasharray=\"{LOGICAL_DASH}\"") } else { String::new() },
        );
        let (first, last) = (points[0], points[points.len() - 1]);
        match self.notation {
            RelationNotation::Plain => {
                let _ = write!(
                    svg,
                    "<circle cx=\"{}\" cy=\"{}\" r=\"3.5\" fill=\"{}\"/>",
                    num(first.x),
                    num(first.y),
                    palette::LINE
                );
                let base = DiagramPoint { x: last.x + directions.1 * 8.0, y: last.y };
                let _ = write!(
                    svg,
                    "<path d=\"M{} {} L{} {} L{} {} Z\" fill=\"{}\"/>",
                    num(last.x),
                    num(last.y),
                    num(base.x),
                    num(base.y - 4.0),
                    num(base.x),
                    num(base.y + 4.0),
                    palette::LINE
                );
            }
            RelationNotation::CrowsFoot => {
                mark_svg(svg, EndMark::child(relation).shape(first, directions.0, 1.0));
                mark_svg(svg, EndMark::parent(relation).shape(last, directions.1, 1.0));
            }
        }
        svg.push_str("</g>\n");
    }

    fn relation_title(&self, relation: &DiagramRelation) -> String {
        let side = |entity: usize, columns: &[usize]| {
            let entity = &self.graph.entities[entity];
            let names: Vec<&str> = columns.iter().map(|&c| entity.columns[c].name.as_str()).collect();
            format!("{}({})", entity.label, names.join(", "))
        };
        format!(
            "{} -> {}",
            side(relation.child.0, &relation.child_columns),
            side(relation.parent.0, &relation.parent_columns)
        )
    }

    fn entity_svg(&self, svg: &mut String, index: usize, entity: &DiagramEntity, rect: DiagramRect) {
        let m = &self.metrics;
        let (x, y, w) = (rect.x, rect.y, rect.width);
        let tinted = self.color_of(entity).map(|color| color.over_white(COLOR_TINT));
        let header_fill = match &tinted {
            Some(color) => color.as_str(),
            None if entity.focus => palette::HEADER_FOCUS,
            None => palette::HEADER,
        };
        let _ = write!(
            svg,
            "<g class=\"entity\" id=\"entity-{index}\"><title>{title}</title>\
             <clipPath id=\"clip-{index}\"><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"4\"/></clipPath>\
             <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"4\" fill=\"{surface}\"/>\
             <g clip-path=\"url(#clip-{index})\">\
             <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{hh}\" fill=\"{header_fill}\"/>\
             <text x=\"{tx}\" y=\"{ty}\" font-size=\"12.5\" font-weight=\"bold\" fill=\"{text}\">{label}</text>",
            title = xml(&entity.label),
            x = num(x),
            y = num(y),
            w = num(w),
            h = num(rect.height),
            hh = num(m.header_height),
            surface = palette::SURFACE,
            tx = num(x + 28.0),
            ty = num(y + m.header_height / 2.0 + 4.5),
            text = palette::TEXT,
            label = xml(&entity.label),
        );
        let _ = write!(
            svg,
            "<rect x=\"{}\" y=\"{}\" width=\"12\" height=\"9\" rx=\"1.5\" fill=\"none\" stroke=\"{}\"/>",
            num(x + 9.0),
            num(y + m.header_height / 2.0 - 4.5),
            if entity.focus { palette::ACCENT } else { palette::FAINT }
        );
        match &entity.state {
            EntityState::Ready => {}
            EntityState::Loading => {
                let _ = write!(
                    svg,
                    "<text x=\"{}\" y=\"{}\" font-size=\"10\" text-anchor=\"end\" fill=\"{}\">loading</text>",
                    num(x + w - 8.0),
                    num(y + m.header_height / 2.0 + 3.5),
                    palette::FAINT
                );
            }
            EntityState::Failed(_) => {
                let _ = write!(
                    svg,
                    "<text x=\"{}\" y=\"{}\" font-size=\"10\" text-anchor=\"end\" fill=\"{}\">failed</text>",
                    num(x + w - 8.0),
                    num(y + m.header_height / 2.0 + 3.5),
                    palette::DANGER
                );
            }
        }
        let mut row_top = y + m.header_height;
        let visible: Vec<&DiagramColumn> = entity.columns.iter().filter(|c| self.mode.shows(c)).collect();
        let pk_rows = visible.iter().take_while(|c| c.primary_key).count();
        for (row, column) in visible.iter().enumerate() {
            if row == pk_rows && pk_rows > 0 {
                let _ = write!(
                    svg,
                    "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\"/>",
                    num(x),
                    num(row_top + m.separator_height / 2.0),
                    num(x + w),
                    num(row_top + m.separator_height / 2.0),
                    palette::BORDER
                );
                row_top += m.separator_height;
            }
            self.row_svg(svg, column, x, row_top, w);
            row_top += m.row_height;
        }
        // The outline goes last so it stays on top of the header fill.
        let _ = writeln!(
            svg,
            "</g><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"4\" fill=\"none\" stroke=\"{}\"/></g>",
            num(x),
            num(y),
            num(w),
            num(rect.height),
            if entity.focus { palette::ACCENT } else { palette::BORDER }
        );
    }

    fn row_svg(&self, svg: &mut String, column: &DiagramColumn, x: f32, top: f32, width: f32) {
        let baseline = top + self.metrics.row_height / 2.0 + 4.0;
        let _ = write!(
            svg,
            "<text x=\"{}\" y=\"{}\" font-size=\"9\" font-weight=\"bold\" text-anchor=\"middle\" fill=\"{}\">{}</text>",
            num(x + 19.0),
            num(baseline),
            palette::FAINT,
            glyph_text(column.glyph)
        );
        let _ = write!(
            svg,
            "<text x=\"{}\" y=\"{}\" font-size=\"11.5\"{} fill=\"{}\">{}</text>",
            num(x + 36.0),
            num(baseline),
            if column.primary_key { " font-weight=\"bold\"" } else { "" },
            palette::TEXT,
            xml(&column.name)
        );
        let marker = if column.primary_key {
            Some(("PK", palette::KEY))
        } else if column.foreign_key {
            Some(("FK", palette::FAINT))
        } else {
            None
        };
        let type_right = if marker.is_some() { width - 26.0 } else { width - 8.0 };
        if !column.data_type.is_empty() {
            let _ = write!(
                svg,
                "<text x=\"{}\" y=\"{}\" font-size=\"10\" text-anchor=\"end\" fill=\"{}\">{}</text>",
                num(x + type_right),
                num(baseline),
                palette::FAINT,
                xml(&column.data_type)
            );
        }
        if let Some((text, color)) = marker {
            let _ = write!(
                svg,
                "<text x=\"{}\" y=\"{}\" font-size=\"8\" font-weight=\"bold\" text-anchor=\"end\" fill=\"{color}\">{text}</text>",
                num(x + width - 6.0),
                num(baseline),
            );
        }
    }
}

fn mark_svg(svg: &mut String, shape: MarkShape) {
    for (a, b) in shape.lines() {
        let _ = write!(
            svg,
            "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"1.25\"/>",
            num(a.x),
            num(a.y),
            num(b.x),
            num(b.y),
            palette::LINE
        );
    }
    if let Some((center, radius)) = shape.ring {
        let _ = write!(
            svg,
            "<circle cx=\"{}\" cy=\"{}\" r=\"{}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"1.25\"/>",
            num(center.x),
            num(center.y),
            num(radius),
            palette::BACKGROUND,
            palette::LINE
        );
    }
}

/// A group's frame with its title (or its collapsed chip).
fn group_svg(svg: &mut String, group: &Group, frame: DiagramRect, members: usize) {
    let color = group.rgb().hex();
    let _ = writeln!(
        svg,
        "<g class=\"group\"><title>{name}</title><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"10\" \
         fill=\"{color}\" fill-opacity=\"{fill}\" stroke=\"{color}\" stroke-opacity=\"0.6\"/>\
         <circle cx=\"{cx}\" cy=\"{cy}\" r=\"4\" fill=\"{color}\"/>\
         <text x=\"{tx}\" y=\"{ty}\" font-size=\"11.5\" font-weight=\"bold\" fill=\"{text}\">{label}</text></g>",
        name = xml(&group.name),
        x = num(frame.x),
        y = num(frame.y),
        w = num(frame.width),
        h = num(frame.height),
        fill = if group.collapsed { "0.22" } else { "0.08" },
        cx = num(frame.x + 14.0),
        cy = num(frame.y + if group.collapsed { frame.height / 2.0 } else { 13.0 }),
        tx = num(frame.x + 24.0),
        ty = num(frame.y + if group.collapsed { frame.height / 2.0 + 4.0 } else { 17.0 }),
        text = palette::TEXT,
        label = xml(&if group.collapsed {
            format!("{} ({members} tables, collapsed)", group.name)
        } else {
            format!("{} ({members})", group.name)
        }),
    );
}

/// Text wrapped to `max_chars` columns (words longer than that are cut).
fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(4);
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let mut word = word.to_string();
            while word.chars().count() > max_chars {
                let head: String = word.chars().take(max_chars).collect();
                word = word.chars().skip(max_chars).collect();
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                lines.push(head);
            }
            if line.is_empty() {
                line = word;
            } else if line.chars().count() + 1 + word.chars().count() <= max_chars {
                line.push(' ');
                line.push_str(&word);
            } else {
                lines.push(std::mem::replace(&mut line, word));
            }
        }
        lines.push(line);
    }
    lines
}

/// A sticky note: a yellow box with its wrapped text.
fn note_svg(svg: &mut String, note: &Note) {
    const FONT_SIZE: f32 = 12.0;
    const LINE_HEIGHT: f32 = 15.0;
    let rect = note.rect();
    let _ = write!(
        svg,
        "<g class=\"note\"><clipPath id=\"note-clip-{id}\"><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\"/></clipPath>\
         <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"3\" fill=\"{fill}\" stroke=\"{stroke}\"/>\
         <g clip-path=\"url(#note-clip-{id})\">",
        id = note.id,
        x = num(rect.x),
        y = num(rect.y),
        w = num(rect.width),
        h = num(rect.height),
        fill = palette::NOTE_FILL,
        stroke = palette::NOTE_BORDER,
    );
    let max_chars = ((rect.width - 16.0) / (FONT_SIZE * 0.56)) as usize;
    for (row, line) in wrap_text(&note.text, max_chars).iter().enumerate() {
        let _ = write!(
            svg,
            "<text x=\"{}\" y=\"{}\" font-size=\"{FONT_SIZE}\" fill=\"{}\" xml:space=\"preserve\">{}</text>",
            num(rect.x + 8.0),
            num(rect.y + 8.0 + FONT_SIZE + row as f32 * LINE_HEIGHT),
            palette::NOTE_TEXT,
            xml(line),
        );
    }
    svg.push_str("</g></g>\n");
}

// ---- PNG -----------------------------------------------------------------------

/// Scale for a `width` x `height` drawing: `PNG_SCALE`, lowered so neither the
/// side nor the pixel count exceeds the caps.
pub(in crate::app) fn png_scale(width: f32, height: f32) -> f32 {
    if width <= 0.0 || height <= 0.0 {
        return PNG_SCALE;
    }
    PNG_SCALE.min(PNG_MAX_SIDE / width).min(PNG_MAX_SIDE / height).min((PNG_MAX_PIXELS / (width * height)).sqrt())
}

fn font_database() -> &'static Arc<resvg::usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut database = resvg::usvg::fontdb::Database::new();
        database.load_system_fonts();
        Arc::new(database)
    })
}

impl ExportRequest {
    fn png(&self) -> Result<Vec<u8>, ExportError> {
        rasterize(&self.svg())
    }
}

/// Renders `svg` at [`png_scale`] to PNG bytes.
pub(in crate::app) fn rasterize(svg: &str) -> Result<Vec<u8>, ExportError> {
    let options =
        resvg::usvg::Options { font_family: "Helvetica".into(), fontdb: font_database().clone(), ..Default::default() };
    let tree = resvg::usvg::Tree::from_str(svg, &options).map_err(ExportError::Svg)?;
    let size = tree.size();
    let scale = png_scale(size.width(), size.height());
    let (width, height) = ((size.width() * scale).ceil() as u32, (size.height() * scale).ceil() as u32);
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width.max(1), height.max(1)).ok_or(ExportError::Pixmap { width, height })?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap.encode_png().map_err(|error| ExportError::Png(error.to_string()))
}

// ---- Mermaid ---------------------------------------------------------------------

fn mermaid_ident(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if cleaned.is_empty() {
        "entity".into()
    } else {
        cleaned
    }
}

impl ExportRequest {
    /// `erDiagram`: parents on the left of `||--o{`; an optional FK makes the
    /// parent end `|o`, a unique FK makes the child end `o|` (one-to-one).
    pub(in crate::app) fn mermaid(&self) -> String {
        let graph = &self.graph;
        let mut out = String::from("erDiagram\n");
        for entity in &graph.entities {
            let _ = writeln!(out, "  {} {{", mermaid_ident(&entity.label));
            for column in &entity.columns {
                let data_type: String = column
                    .data_type
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("_")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                let keys = match (column.primary_key, column.foreign_key) {
                    (true, true) => " PK, FK",
                    (true, false) => " PK",
                    (false, true) => " FK",
                    (false, false) => "",
                };
                let _ = writeln!(
                    out,
                    "    {} {}{keys}",
                    if data_type.is_empty() { "unknown" } else { &data_type },
                    mermaid_ident(&column.name)
                );
            }
            out.push_str("  }\n");
        }
        for relation in &graph.relations {
            let (child, parent) = (&graph.entities[relation.child.0], &graph.entities[relation.parent.0]);
            let parent_end = if relation.optional { "|o" } else { "||" };
            let child_end = if relation.unique { "o|" } else { "o{" };
            // A logical relationship is a non-identifying (dashed) one.
            let (link, tag) = match relation.origin {
                RelationOrigin::Logical(_) => ("..", " (logical)"),
                RelationOrigin::Declared | RelationOrigin::Draft => ("--", ""),
            };
            let label = fk_label(child, &relation.child_columns, parent, &relation.parent_columns).replace('"', "'");
            let _ = writeln!(
                out,
                "  {} {parent_end}{link}{child_end} {} : \"{label}{tag}\"",
                mermaid_ident(&parent.label),
                mermaid_ident(&child.label)
            );
        }
        // Mermaid has no notes, colors or groups for ER diagrams: comments
        // keep them with the file.
        let annotations = &self.annotations;
        for group in &annotations.groups {
            let tables: Vec<String> = group.members.iter().map(|m| mermaid_ident(&m.table)).collect();
            let _ = writeln!(out, "  %% group {}: {}", group.name.replace('\n', " "), tables.join(", "));
        }
        for (table, color) in &annotations.colors {
            let _ = writeln!(out, "  %% color {}: {}", mermaid_ident(&table.table), color.hex());
        }
        for note in &annotations.notes {
            for line in note.text.lines() {
                let _ = writeln!(out, "  %% note {}: {line}", note.id);
            }
        }
        out
    }
}

/// `child_col -> parent_col`, comma separated for composite keys.
fn fk_label(
    child: &DiagramEntity,
    child_columns: &[usize],
    parent: &DiagramEntity,
    parent_columns: &[usize],
) -> String {
    let names = |entity: &DiagramEntity, columns: &[usize]| {
        columns.iter().map(|&c| entity.columns[c].name.as_str()).collect::<Vec<_>>().join(", ")
    };
    format!("{} -> {}", names(child, child_columns), names(parent, parent_columns))
}

// ---- DBML -----------------------------------------------------------------------

fn dbml_ident(name: &str) -> String {
    let simple = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if simple {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn dbml_type(data_type: &str) -> String {
    let t = data_type.trim();
    let t = if t.is_empty() { "varchar" } else { t };
    let mut chars = t.chars();
    let head_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let plain = head_ok && {
        let base = t.split('(').next().unwrap_or(t);
        base.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && (t.len() == base.len() || (t.ends_with(')') && t.matches('(').count() == 1))
    };
    if plain {
        t.to_string()
    } else {
        format!("\"{}\"", t.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn dbml_table(entity: &DiagramEntity) -> String {
    match entity.table.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(schema) => format!("{}.{}", dbml_ident(schema), dbml_ident(&entity.table.table)),
        None => dbml_ident(&entity.table.table),
    }
}

impl ExportRequest {
    /// Tables, then one `Ref` per relation with known columns (`>` many to
    /// one, `-` one to one).
    pub(in crate::app) fn dbml(&self) -> String {
        let graph = &self.graph;
        let mut out = String::from("// Generated by TableX\n\n");
        for entity in &graph.entities {
            let header_color = self.color_of(entity).map(|color| format!(" [headercolor: {}]", color.hex())).unwrap_or_default();
            let _ = writeln!(out, "Table {}{header_color} {{", dbml_table(entity));
            for column in &entity.columns {
                let mut settings = Vec::new();
                if column.primary_key {
                    settings.push("pk");
                }
                if !column.nullable {
                    settings.push("not null");
                }
                let suffix = if settings.is_empty() { String::new() } else { format!(" [{}]", settings.join(", ")) };
                let _ = writeln!(out, "  {} {}{suffix}", dbml_ident(&column.name), dbml_type(&column.data_type));
            }
            out.push_str("}\n\n");
        }
        for relation in &graph.relations {
            let (child, parent) = (&graph.entities[relation.child.0], &graph.entities[relation.parent.0]);
            if relation.child_columns.is_empty() || relation.child_columns.len() != relation.parent_columns.len() {
                continue;
            }
            let side = |entity: &DiagramEntity, columns: &[usize]| {
                let names: Vec<String> = columns.iter().map(|&c| dbml_ident(&entity.columns[c].name)).collect();
                match names.as_slice() {
                    [one] => format!("{}.{one}", dbml_table(entity)),
                    many => format!("{}.({})", dbml_table(entity), many.join(", ")),
                }
            };
            let _ = writeln!(
                out,
                "Ref: {} {} {}{}",
                side(child, &relation.child_columns),
                if relation.unique { "-" } else { ">" },
                side(parent, &relation.parent_columns),
                if matches!(relation.origin, RelationOrigin::Logical(_)) { " // logical relationship" } else { "" },
            );
        }
        let groups = GroupIndex::build(graph, &self.annotations.groups);
        for (group, members) in self.annotations.groups.iter().zip(&groups.members) {
            let _ = writeln!(out, "\nTableGroup {} {{", dbml_ident(&group.name));
            for &member in members {
                let _ = writeln!(out, "  {}", dbml_table(&graph.entities[member]));
            }
            out.push_str("}\n");
        }
        for note in &self.annotations.notes {
            let _ = writeln!(out, "\nNote note_{} {{\n  '''{}'''\n}}", note.id, note.text.replace('\\', "\\\\").replace('\'', "\\\'"));
        }
        out
    }
}

// ---- JSON -----------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonSnapshot<'a> {
    format: &'static str,
    version: u32,
    /// `table` (one focus table and its neighbours) or `schema`.
    kind: &'static str,
    database: &'a str,
    schema: Option<&'a str>,
    tables: Vec<JsonTable<'a>>,
    relations: Vec<JsonRelation<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<JsonNote<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    groups: Vec<JsonGroup<'a>>,
}

#[derive(Serialize)]
struct JsonNote<'a> {
    id: u64,
    text: &'a str,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Serialize)]
struct JsonGroup<'a> {
    name: &'a str,
    color: &'a str,
    hidden: bool,
    collapsed: bool,
    tables: Vec<JsonTableRef<'a>>,
}

#[derive(Serialize)]
struct JsonTableRef<'a> {
    schema: Option<&'a str>,
    table: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonTable<'a> {
    name: &'a str,
    schema: Option<&'a str>,
    focus: bool,
    position: JsonRect,
    /// `#rrggbb` header color, when the user set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    columns: Vec<JsonColumn<'a>>,
}

#[derive(Serialize)]
struct JsonRect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonColumn<'a> {
    name: &'a str,
    data_type: &'a str,
    nullable: bool,
    primary_key: bool,
    foreign_key: bool,
}

#[derive(Serialize)]
struct JsonRelation<'a> {
    name: &'a str,
    child: JsonEnd<'a>,
    parent: JsonEnd<'a>,
    optional: bool,
    unique: bool,
    /// A user-declared relationship (not a foreign key of the database).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    logical: bool,
}

#[derive(Serialize)]
struct JsonEnd<'a> {
    schema: Option<&'a str>,
    table: &'a str,
    columns: Vec<&'a str>,
}

impl ExportRequest {
    pub(in crate::app) fn json(&self) -> Result<String, ExportError> {
        let graph = &self.graph;
        let first = graph.entities.first().map(|e| &e.table);
        fn end<'a>(entity: &'a DiagramEntity, columns: &[usize]) -> JsonEnd<'a> {
            JsonEnd {
                schema: entity.table.schema.as_deref(),
                table: &entity.table.table,
                columns: columns.iter().map(|&c| entity.columns[c].name.as_str()).collect(),
            }
        }
        let snapshot = JsonSnapshot {
            format: "tablex-diagram",
            version: 1,
            kind: if graph.entities.iter().any(|e| e.focus) { "table" } else { "schema" },
            database: first.map_or("", |t| t.database.as_str()),
            schema: first.and_then(|t| t.schema.as_deref()),
            tables: graph
                .entities
                .iter()
                .zip(&self.layout.rects)
                .map(|(entity, rect)| JsonTable {
                    name: &entity.table.table,
                    schema: entity.table.schema.as_deref(),
                    focus: entity.focus,
                    position: JsonRect { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
                    color: self.color_of(entity).map(Rgb::hex),
                    columns: entity
                        .columns
                        .iter()
                        .map(|c| JsonColumn {
                            name: &c.name,
                            data_type: &c.data_type,
                            nullable: c.nullable,
                            primary_key: c.primary_key,
                            foreign_key: c.foreign_key,
                        })
                        .collect(),
                })
                .collect(),
            relations: graph
                .relations
                .iter()
                .map(|r| JsonRelation {
                    name: &r.name,
                    child: end(&graph.entities[r.child.0], &r.child_columns),
                    parent: end(&graph.entities[r.parent.0], &r.parent_columns),
                    optional: r.optional,
                    unique: r.unique,
                    logical: matches!(r.origin, RelationOrigin::Logical(_)),
                })
                .collect(),
            notes: self
                .annotations
                .notes
                .iter()
                .map(|n| {
                    let rect = n.rect();
                    JsonNote { id: n.id, text: &n.text, x: rect.x, y: rect.y, width: rect.width, height: rect.height }
                })
                .collect(),
            groups: self
                .annotations
                .groups
                .iter()
                .map(|g| JsonGroup {
                    name: &g.name,
                    color: &g.color,
                    hidden: g.hidden,
                    collapsed: g.collapsed,
                    tables: g
                        .members
                        .iter()
                        .map(|m| JsonTableRef { schema: m.schema.as_deref(), table: &m.table })
                        .collect(),
                })
                .collect(),
        };
        let mut text = serde_json::to_string_pretty(&snapshot).map_err(ExportError::Json)?;
        text.push('\n');
        Ok(text)
    }
}
