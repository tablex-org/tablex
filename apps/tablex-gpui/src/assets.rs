use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(asset(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path == "icons" {
            Ok(vec![
                "building-2.svg",
                "check.svg",
                "chevron-down.svg",
                "chevron-left.svg",
                "chevron-right.svg",
                "case-sensitive.svg",
                "replace.svg",
                "chevrons-up-down.svg",
                "chevrons-left.svg",
                "close.svg",
                "copy.svg",
                "delete.svg",
                "file.svg",
                "file-play.svg",
                "filter.svg",
                "folder-closed.svg",
                "folder-open.svg",
                "frame.svg",
                "pencil.svg",
                "undo-2.svg",
                "redo-2.svg",
                "layout-dashboard.svg",
                "list-checks.svg",
                "loader-circle.svg",
                "maximize.svg",
                "mariadb.svg",
                "clickhouse.svg",
                "mongodb.svg",
                "redis.svg",
                "mysql.svg",
                "mysql-mark.svg",
                "plus.svg",
                "panel-bottom.svg",
                "panel-right.svg",
                "play.svg",
                "postgres.svg",
                "search.svg",
                "settings-2.svg",
                "settings.svg",
                "sql-server.svg",
                "square-terminal.svg",
                "sqlite.svg",
                "oracle.svg",
                "firebird.svg",
                "cockroachlabs.svg",
                "cloud-spanner.svg",
                "database.svg",
                "table.svg",
                "table-2.svg",
                "braces.svg",
                "link.svg",
                "minus.svg",
                "dolt.svg",
                "iris.svg",
                "informix.svg",
                "db2.svg",
                "pipette.svg",
                "ellipsis.svg",
                "circle-help.svg",
                "eye.svg",
                "eye-off.svg",
                "grip-vertical.svg",
                "refresh-cw.svg",
                "locate-fixed.svg",
                "folder-plus.svg",
                "upload.svg",
                "download.svg",
                "chevrons-down-up.svg",
                "arrow-down-up.svg",
                "list-ordered.svg",
                "arrow-down-a-z.svg",
                "arrow-up-z-a.svg",
                "plug.svg",
                "folder-input.svg",
                "trash.svg",
                "list-checks.svg",
                "active-dot.svg",
                "active-ring.svg",
                "sliders-horizontal.svg",
                "regex.svg",
                "eraser.svg",
                "arrow-up.svg",
                "arrow-down.svg",
                "columns-3.svg",
                "rows-3.svg",
                "circle-x.svg",
                "wand-sparkles.svg",
                "type.svg",
                "calendar-clock.svg",
                "calendar-days.svg",
                "key-round.svg",
                "hash.svg",
                "snowflake.svg",
                "camera.svg",
                "circle-plus.svg",
                "square-dashed.svg",
                "arrow-left-right.svg",
                "pin.svg",
                "minimize-2.svg",
                "maximize-2.svg",
                "crosshair.svg",
                "copy-plus.svg",
                "pencil-ruler.svg",
                "table-properties.svg",
                "triangle-alert.svg",
                "info.svg",
                "network.svg",
                "chart-column-big.svg",
            ]
            .into_iter()
            .map(SharedString::from)
            .collect())
        } else {
            Ok(Vec::new())
        }
    }
}

pub fn asset(path: &str) -> Option<&'static [u8]> {
    match path {
        "icons/building-2.svg" => Some(include_bytes!("../assets/icons/building-2.svg")),
        "icons/check.svg" => Some(include_bytes!("../assets/icons/check.svg")),
        "icons/chevron-down.svg" => Some(include_bytes!("../assets/icons/chevron-down.svg")),
        "icons/chevron-left.svg" => Some(include_bytes!("../assets/icons/chevron-left.svg")),
        "icons/chevron-right.svg" => Some(include_bytes!("../assets/icons/chevron-right.svg")),
        "icons/case-sensitive.svg" => Some(include_bytes!("../assets/icons/case-sensitive.svg")),
        "icons/replace.svg" => Some(include_bytes!("../assets/icons/replace.svg")),
        "icons/chevrons-up-down.svg" => {
            Some(include_bytes!("../assets/icons/chevrons-up-down.svg"))
        }
        "icons/triangle-alert.svg" => Some(include_bytes!("../assets/icons/triangle-alert.svg")),
        "icons/info.svg" => Some(include_bytes!("../assets/icons/info.svg")),
        "icons/close.svg" => Some(include_bytes!("../assets/icons/close.svg")),
        "icons/copy.svg" => Some(include_bytes!("../assets/icons/copy.svg")),
        "icons/delete.svg" => Some(include_bytes!("../assets/icons/delete.svg")),
        "icons/file.svg" => Some(include_bytes!("../assets/icons/file.svg")),
        "icons/file-play.svg" => Some(include_bytes!("../assets/icons/file-play.svg")),
        "icons/eye-off.svg" => Some(include_bytes!("../assets/icons/eye-off.svg")),
        "icons/grip-vertical.svg" => Some(include_bytes!("../assets/icons/grip-vertical.svg")),
        "icons/filter.svg" => Some(include_bytes!("../assets/icons/filter.svg")),
        "icons/folder-closed.svg" => Some(include_bytes!("../assets/icons/folder-closed.svg")),
        "icons/folder-open.svg" => Some(include_bytes!("../assets/icons/folder-open.svg")),
        "icons/frame.svg" => Some(include_bytes!("../assets/icons/frame.svg")),
        "icons/pencil.svg" => Some(include_bytes!("../assets/icons/pencil.svg")),
        "icons/undo-2.svg" => Some(include_bytes!("../assets/icons/undo-2.svg")),
        "icons/redo-2.svg" => Some(include_bytes!("../assets/icons/redo-2.svg")),
        "icons/layout-dashboard.svg" => {
            Some(include_bytes!("../assets/icons/layout-dashboard.svg"))
        }
        "icons/list-checks.svg" => Some(include_bytes!("../assets/icons/list-checks.svg")),
        "icons/loader-circle.svg" => Some(include_bytes!("../assets/icons/loader-circle.svg")),
        "icons/maximize.svg" => Some(include_bytes!("../assets/icons/maximize.svg")),
        "icons/mariadb.svg" => Some(include_bytes!("../assets/icons/mariadb.svg")),
        "icons/clickhouse.svg" => Some(include_bytes!("../assets/icons/clickhouse.svg")),
        "icons/plus.svg" => Some(include_bytes!("../assets/icons/plus.svg")),
        "icons/panel-bottom.svg" => Some(include_bytes!("../assets/icons/panel-bottom.svg")),
        "icons/panel-right.svg" => Some(include_bytes!("../assets/icons/panel-right.svg")),
        "icons/play.svg" => Some(include_bytes!("../assets/icons/play.svg")),
        "icons/postgres.svg" => Some(include_bytes!("../assets/icons/postgres.svg")),
        "icons/search.svg" => Some(include_bytes!("../assets/icons/search.svg")),
        "icons/settings-2.svg" => Some(include_bytes!("../assets/icons/settings-2.svg")),
        "icons/settings.svg" => Some(include_bytes!("../assets/icons/settings.svg")),
        "icons/sql-server.svg" => Some(include_bytes!("../assets/icons/sql-server.svg")),
        "icons/square-terminal.svg" => Some(include_bytes!("../assets/icons/square-terminal.svg")),
        "icons/sqlite.svg" => Some(include_bytes!("../assets/icons/sqlite.svg")),
        "icons/mongodb.svg" => Some(include_bytes!("../assets/icons/mongodb.svg")),
        "icons/redis.svg" => Some(include_bytes!("../assets/icons/redis.svg")),
        "icons/mysql.svg" => Some(include_bytes!("../assets/icons/mysql.svg")),
        "icons/mysql-mark.svg" => Some(include_bytes!("../assets/icons/mysql-mark.svg")),
        "icons/oracle.svg" => Some(include_bytes!("../assets/icons/oracle.svg")),
        "icons/firebird.svg" => Some(include_bytes!("../assets/icons/firebird.svg")),
        "icons/cockroachlabs.svg" => Some(include_bytes!("../assets/icons/cockroachlabs.svg")),
        "icons/cloud-spanner.svg" => Some(include_bytes!("../assets/icons/cloud-spanner.svg")),
        "icons/database.svg" => Some(include_bytes!("../assets/icons/database.svg")),
        "icons/table.svg" => Some(include_bytes!("../assets/icons/table.svg")),
        "icons/table-2.svg" => Some(include_bytes!("../assets/icons/table-2.svg")),
        "icons/braces.svg" => Some(include_bytes!("../assets/icons/braces.svg")),
        "icons/link.svg" => Some(include_bytes!("../assets/icons/link.svg")),
        "icons/minus.svg" => Some(include_bytes!("../assets/icons/minus.svg")),
        "icons/dolt.svg" => Some(include_bytes!("../assets/icons/dolt.svg")),
        "icons/iris.svg" => Some(include_bytes!("../assets/icons/iris.svg")),
        "icons/informix.svg" => Some(include_bytes!("../assets/icons/informix.svg")),
        "icons/db2.svg" => Some(include_bytes!("../assets/icons/db2.svg")),
        "icons/pipette.svg" => Some(include_bytes!("../assets/icons/pipette.svg")),
        "icons/ellipsis.svg" => Some(include_bytes!("../assets/icons/ellipsis.svg")),
        "icons/circle-help.svg" => Some(include_bytes!("../assets/icons/circle-help.svg")),
        "icons/eye.svg" => Some(include_bytes!("../assets/icons/eye.svg")),
        "icons/refresh-cw.svg" => Some(include_bytes!("../assets/icons/refresh-cw.svg")),
        "icons/locate-fixed.svg" => Some(include_bytes!("../assets/icons/locate-fixed.svg")),
        "icons/folder-plus.svg" => Some(include_bytes!("../assets/icons/folder-plus.svg")),
        "icons/upload.svg" => Some(include_bytes!("../assets/icons/upload.svg")),
        "icons/download.svg" => Some(include_bytes!("../assets/icons/download.svg")),
        "icons/chevrons-down-up.svg" => {
            Some(include_bytes!("../assets/icons/chevrons-down-up.svg"))
        }
        "icons/chevrons-left.svg" => Some(include_bytes!("../assets/icons/chevrons-left.svg")),
        "icons/arrow-down-up.svg" => Some(include_bytes!("../assets/icons/arrow-down-up.svg")),
        "icons/list-ordered.svg" => Some(include_bytes!("../assets/icons/list-ordered.svg")),
        "icons/arrow-down-a-z.svg" => Some(include_bytes!("../assets/icons/arrow-down-a-z.svg")),
        "icons/arrow-up-z-a.svg" => Some(include_bytes!("../assets/icons/arrow-up-z-a.svg")),
        "icons/plug.svg" => Some(include_bytes!("../assets/icons/plug.svg")),
        "icons/folder-input.svg" => Some(include_bytes!("../assets/icons/folder-input.svg")),
        "icons/trash.svg" => Some(include_bytes!("../assets/icons/trash.svg")),
        "icons/active-dot.svg" => Some(include_bytes!("../assets/icons/active-dot.svg")),
        "icons/active-ring.svg" => Some(include_bytes!("../assets/icons/active-ring.svg")),
        "icons/sliders-horizontal.svg" => {
            Some(include_bytes!("../assets/icons/sliders-horizontal.svg"))
        }
        "icons/regex.svg" => Some(include_bytes!("../assets/icons/regex.svg")),
        "icons/eraser.svg" => Some(include_bytes!("../assets/icons/eraser.svg")),
        "icons/arrow-up.svg" => Some(include_bytes!("../assets/icons/arrow-up.svg")),
        "icons/arrow-down.svg" => Some(include_bytes!("../assets/icons/arrow-down.svg")),
        "icons/columns-3.svg" => Some(include_bytes!("../assets/icons/columns-3.svg")),
        "icons/rows-3.svg" => Some(include_bytes!("../assets/icons/rows-3.svg")),
        "icons/circle-x.svg" => Some(include_bytes!("../assets/icons/circle-x.svg")),
        "icons/wand-sparkles.svg" => Some(include_bytes!("../assets/icons/wand-sparkles.svg")),
        "icons/type.svg" => Some(include_bytes!("../assets/icons/type.svg")),
        "icons/calendar-clock.svg" => Some(include_bytes!("../assets/icons/calendar-clock.svg")),
        "icons/calendar-days.svg" => Some(include_bytes!("../assets/icons/calendar-days.svg")),
        "icons/copy-plus.svg" => Some(include_bytes!("../assets/icons/copy-plus.svg")),
        "icons/pencil-ruler.svg" => Some(include_bytes!("../assets/icons/pencil-ruler.svg")),
        "icons/table-properties.svg" => {
            Some(include_bytes!("../assets/icons/table-properties.svg"))
        }
        "icons/key-round.svg" => Some(include_bytes!("../assets/icons/key-round.svg")),
        "icons/hash.svg" => Some(include_bytes!("../assets/icons/hash.svg")),
        "icons/snowflake.svg" => Some(include_bytes!("../assets/icons/snowflake.svg")),
        "icons/camera.svg" => Some(include_bytes!("../assets/icons/camera.svg")),
        "icons/circle-plus.svg" => Some(include_bytes!("../assets/icons/circle-plus.svg")),
        "icons/square-dashed.svg" => Some(include_bytes!("../assets/icons/square-dashed.svg")),
        "icons/arrow-left-right.svg" => {
            Some(include_bytes!("../assets/icons/arrow-left-right.svg"))
        }
        "icons/pin.svg" => Some(include_bytes!("../assets/icons/pin.svg")),
        "icons/minimize-2.svg" => Some(include_bytes!("../assets/icons/minimize-2.svg")),
        "icons/maximize-2.svg" => Some(include_bytes!("../assets/icons/maximize-2.svg")),
        "icons/crosshair.svg" => Some(include_bytes!("../assets/icons/crosshair.svg")),
        "icons/network.svg" => Some(include_bytes!("../assets/icons/network.svg")),
        "icons/chart-column-big.svg" => Some(include_bytes!("../assets/icons/chart-column-big.svg")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn component_icon_assets_are_available() {
        assert!(super::asset("icons/plus.svg").is_some());
        assert!(super::asset("icons/building-2.svg").is_some());
        assert!(super::asset("icons/chart-column-big.svg").is_some());
        assert!(super::asset("icons/filter.svg").is_some());
        assert!(super::asset("icons/play.svg").is_some());
        assert!(super::asset("icons/chevron-down.svg").is_some());
        assert!(super::asset("icons/chevron-right.svg").is_some());
        assert!(super::asset("icons/panel-bottom.svg").is_some());
        assert!(super::asset("icons/panel-right.svg").is_some());
        assert!(super::asset("icons/check.svg").is_some());
        assert!(super::asset("icons/copy.svg").is_some());
        assert!(super::asset("icons/delete.svg").is_some());
        assert!(super::asset("icons/pencil.svg").is_some());
        assert!(super::asset("icons/undo-2.svg").is_some());
        assert!(super::asset("icons/redo-2.svg").is_some());
        assert!(super::asset("icons/maximize.svg").is_some());
        assert!(super::asset("icons/search.svg").is_some());
        assert!(super::asset("icons/settings-2.svg").is_some());
        assert!(super::asset("icons/settings.svg").is_some());
        assert!(super::asset("icons/list-checks.svg").is_some());
        assert!(super::asset("icons/postgres.svg").is_some());
        assert!(super::asset("icons/sqlite.svg").is_some());
        assert!(super::asset("icons/mongodb.svg").is_some());
        assert!(super::asset("icons/mysql.svg").is_some());
        assert!(super::asset("icons/mariadb.svg").is_some());
        assert!(super::asset("icons/clickhouse.svg").is_some());
        assert!(super::asset("icons/sql-server.svg").is_some());
        assert!(super::asset("icons/oracle.svg").is_some());
        assert!(super::asset("icons/firebird.svg").is_some());
        assert!(super::asset("icons/cockroachlabs.svg").is_some());
        assert!(super::asset("icons/cloud-spanner.svg").is_some());
        assert!(super::asset("icons/database.svg").is_some());
        assert!(super::asset("icons/table.svg").is_some());
        assert!(super::asset("icons/eye.svg").is_some());
        assert!(super::asset("icons/refresh-cw.svg").is_some());
        assert!(super::asset("icons/locate-fixed.svg").is_some());
        assert!(super::asset("icons/folder-plus.svg").is_some());
        assert!(super::asset("icons/upload.svg").is_some());
        assert!(super::asset("icons/download.svg").is_some());
        assert!(super::asset("icons/chevrons-down-up.svg").is_some());
        assert!(super::asset("icons/chevrons-left.svg").is_some());
        assert!(super::asset("icons/arrow-down-up.svg").is_some());
        assert!(super::asset("icons/list-ordered.svg").is_some());
        assert!(super::asset("icons/arrow-down-a-z.svg").is_some());
        assert!(super::asset("icons/arrow-up-z-a.svg").is_some());
        assert!(super::asset("icons/plug.svg").is_some());
        assert!(super::asset("icons/folder-input.svg").is_some());
        assert!(super::asset("icons/trash.svg").is_some());
        assert!(super::asset("icons/list-checks.svg").is_some());
        assert!(super::asset("icons/active-dot.svg").is_some());
        assert!(super::asset("icons/active-ring.svg").is_some());
        assert!(super::asset("icons/regex.svg").is_some());
        assert!(super::asset("icons/sliders-horizontal.svg").is_some());
        assert!(super::asset("icons/eraser.svg").is_some());
        assert!(super::asset("icons/arrow-up.svg").is_some());
        assert!(super::asset("icons/arrow-down.svg").is_some());
        assert!(super::asset("icons/columns-3.svg").is_some());
        assert!(super::asset("icons/rows-3.svg").is_some());
        assert!(super::asset("icons/circle-x.svg").is_some());
        assert!(super::asset("icons/wand-sparkles.svg").is_some());
        assert!(super::asset("icons/type.svg").is_some());
        assert!(super::asset("icons/calendar-clock.svg").is_some());
        assert!(super::asset("icons/calendar-days.svg").is_some());
        assert!(super::asset("icons/key-round.svg").is_some());
        assert!(super::asset("icons/copy-plus.svg").is_some());
        assert!(super::asset("icons/pencil-ruler.svg").is_some());
        assert!(super::asset("icons/table-properties.svg").is_some());
        assert!(super::asset("icons/hash.svg").is_some());
        assert!(super::asset("icons/snowflake.svg").is_some());
        assert!(super::asset("icons/camera.svg").is_some());
        assert!(super::asset("icons/circle-plus.svg").is_some());
        assert!(super::asset("icons/square-dashed.svg").is_some());
        assert!(super::asset("icons/arrow-left-right.svg").is_some());
        assert!(super::asset("icons/network.svg").is_some());
    }

    /// WCAG contrast of an asset's fill against an opaque `surface` (0..=255 RGB).
    fn fill_contrast(path: &str, surface: [f32; 3]) -> f32 {
        let svg = std::str::from_utf8(super::asset(path).expect("asset should be embedded"))
            .expect("SVG asset should be UTF-8");
        let fill = svg
            .split("fill=\"")
            .nth(1)
            .and_then(|value| value.get(..7))
            .expect("brand asset should define a fill color");
        let rgb = u32::from_str_radix(&fill[1..], 16).expect("fill should be a hex color");
        let luminance = |channel: f32| {
            let channel = channel / 255.0;
            if channel <= 0.03928 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        let relative = |[red, green, blue]: [f32; 3]| {
            0.2126 * luminance(red) + 0.7152 * luminance(green) + 0.0722 * luminance(blue)
        };
        let asset = relative([
            ((rgb >> 16) & 0xff) as f32,
            ((rgb >> 8) & 0xff) as f32,
            (rgb & 0xff) as f32,
        ]);
        let surface = relative(surface);
        (asset.max(surface) + 0.05) / (asset.min(surface) + 0.05)
    }

    #[test]
    fn dark_picker_brand_assets_have_visible_fills() {
        for path in [
            "icons/mysql.svg",
            "icons/mysql-mark.svg",
            "icons/mariadb.svg",
            "icons/clickhouse.svg",
            "icons/cockroachlabs.svg",
            "icons/cloud-spanner.svg",
        ] {
            assert!(
                fill_contrast(path, [31.0, 32.0, 35.0]) >= 3.0,
                "brand asset has insufficient contrast on the dark picker surface: {path}"
            );
        }
    }

    /// The single-color marks used to be near-white; they carry a brand color
    /// that reads on the Light picker surface as well as the Dark one.
    #[test]
    fn single_color_brand_assets_are_also_visible_on_the_light_surface() {
        for path in ["icons/mysql.svg", "icons/mysql-mark.svg", "icons/mariadb.svg", "icons/cockroachlabs.svg"] {
            assert!(
                fill_contrast(path, [244.0, 245.0, 247.0]) >= 3.0,
                "brand asset has insufficient contrast on the light picker surface: {path}"
            );
        }
    }

    #[test]
    fn icon_assets_are_listed() {
        let names = <super::Assets as gpui::AssetSource>::list(&super::Assets, "icons")
            .expect("icon listing should be available");
        assert!(names.iter().any(|name| name == "plus.svg"));
        assert!(names.iter().any(|name| name == "filter.svg"));
        for icon in ["eye", "eye-off", "grip-vertical"] {
            assert!(names.iter().any(|name| *name == format!("{icon}.svg")));
            assert!(super::asset(&format!("icons/{icon}.svg")).is_some());
        }
        // The icons of the code editor's built-in search panel (gpui-component draws them by path).
        for icon in ["case-sensitive", "replace", "chevron-left", "chevron-right", "close"] {
            assert!(names.iter().any(|name| *name == format!("{icon}.svg")));
            assert!(super::asset(&format!("icons/{icon}.svg")).is_some());
        }
        assert!(names.iter().any(|name| name == "play.svg"));
        assert!(names.iter().any(|name| name == "chevron-down.svg"));
        assert!(names.iter().any(|name| name == "chevron-right.svg"));
        assert!(names.iter().any(|name| name == "panel-bottom.svg"));
        assert!(names.iter().any(|name| name == "panel-right.svg"));
        assert!(names.iter().any(|name| name == "check.svg"));
        assert!(names.iter().any(|name| name == "delete.svg"));
        assert!(names.iter().any(|name| name == "maximize.svg"));
        assert!(names.iter().any(|name| name == "search.svg"));
        assert!(names.iter().any(|name| name == "settings-2.svg"));
        assert!(names.iter().any(|name| name == "settings.svg"));
        assert!(names.iter().any(|name| name == "list-checks.svg"));
        assert!(names.iter().any(|name| name == "postgres.svg"));
        assert!(names.iter().any(|name| name == "sqlite.svg"));
        assert!(names.iter().any(|name| name == "mongodb.svg"));
        assert!(names.iter().any(|name| name == "mysql.svg"));
        assert!(names.iter().any(|name| name == "mysql-mark.svg"));
        assert!(names.iter().any(|name| name == "mariadb.svg"));
        assert!(names.iter().any(|name| name == "clickhouse.svg"));
        assert!(names.iter().any(|name| name == "sql-server.svg"));
        assert!(names.iter().any(|name| name == "oracle.svg"));
        assert!(names.iter().any(|name| name == "firebird.svg"));
        assert!(names.iter().any(|name| name == "cockroachlabs.svg"));
        assert!(names.iter().any(|name| name == "cloud-spanner.svg"));
        assert!(names.iter().any(|name| name == "database.svg"));
        assert!(names.iter().any(|name| name == "table.svg"));
        assert!(names.iter().any(|name| name == "table-2.svg"));
        assert!(names.iter().any(|name| name == "braces.svg"));
        for icon in ["link", "minus", "ellipsis", "pipette", "circle-help", "db2", "informix", "iris", "dolt"] {
            assert!(names.iter().any(|name| *name == format!("{icon}.svg")));
            assert!(super::asset(&format!("icons/{icon}.svg")).is_some());
        }
        assert!(super::asset("icons/table-2.svg").is_some());
        assert!(super::asset("icons/braces.svg").is_some());
        assert!(names.iter().any(|name| name == "refresh-cw.svg"));
        assert!(names.iter().any(|name| name == "locate-fixed.svg"));
        assert!(names.iter().any(|name| name == "arrow-down-up.svg"));
        assert!(names.iter().any(|name| name == "plug.svg"));
        assert!(names.iter().any(|name| name == "folder-input.svg"));
        assert!(names.iter().any(|name| name == "file-play.svg"));
        assert!(super::asset("icons/file-play.svg").is_some());
        assert!(names.iter().any(|name| name == "sliders-horizontal.svg"));
        assert!(names.iter().any(|name| name == "regex.svg"));
        assert!(names.iter().any(|name| name == "eraser.svg"));
        assert!(names.iter().any(|name| name == "arrow-up.svg"));
        assert!(names.iter().any(|name| name == "arrow-down.svg"));
        assert!(names.iter().any(|name| name == "columns-3.svg"));
        assert!(names.iter().any(|name| name == "rows-3.svg"));
        assert!(names.iter().any(|name| name == "circle-x.svg"));
        assert!(names.iter().any(|name| name == "wand-sparkles.svg"));
        assert!(names.iter().any(|name| name == "type.svg"));
        assert!(names.iter().any(|name| name == "calendar-clock.svg"));
        assert!(names.iter().any(|name| name == "calendar-days.svg"));
        assert!(names.iter().any(|name| name == "key-round.svg"));
        assert!(names.iter().any(|name| name == "hash.svg"));
        assert!(names.iter().any(|name| name == "snowflake.svg"));
        assert!(names.iter().any(|name| name == "camera.svg"));
        assert!(names.iter().any(|name| name == "circle-plus.svg"));
        assert!(names.iter().any(|name| name == "square-dashed.svg"));
        assert!(names.iter().any(|name| name == "arrow-left-right.svg"));
        for icon in ["triangle-alert", "info"] {
            assert!(names.iter().any(|name| *name == format!("{icon}.svg")));
            assert!(super::asset(&format!("icons/{icon}.svg")).is_some());
        }
    }
}
