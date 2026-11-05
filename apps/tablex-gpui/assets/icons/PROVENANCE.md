# Connection picker logos

These SVGs are vendored for local GPUI rendering. They contain only inline SVG
geometry; there are no scripts, stylesheets, images, or remote references.

| Picker entry | Local asset | Public source (pinned ref) | License |
| --- | --- | --- | --- |
| MySQL, Custom (MySQL) | `mysql.svg` | [simple-icons/icons/mysql.svg](https://github.com/simple-icons/simple-icons/blob/5d5d4d1d28cbb00b21770bb69d8112da52211a95/icons/mysql.svg) | [CC0 1.0](https://github.com/simple-icons/simple-icons/blob/5d5d4d1d28cbb00b21770bb69d8112da52211a95/LICENSE.md) |
| MariaDB | `mariadb.svg` | [simple-icons/icons/mariadb.svg](https://github.com/simple-icons/simple-icons/blob/5d5d4d1d28cbb00b21770bb69d8112da52211a95/icons/mariadb.svg) | [CC0 1.0](https://github.com/simple-icons/simple-icons/blob/5d5d4d1d28cbb00b21770bb69d8112da52211a95/LICENSE.md) |
| SQL Server | `sql-server.svg` | [devicon/icons/microsoftsqlserver/microsoftsqlserver-plain.svg](https://github.com/devicons/devicon/blob/7330accdbc47e2dc0c19789a48533c4a3c50fe58/icons/microsoftsqlserver/microsoftsqlserver-plain.svg) | [MIT](https://github.com/devicons/devicon/blob/7330accdbc47e2dc0c19789a48533c4a3c50fe58/LICENSE) |
| Oracle | `oracle.svg` | [devicon/icons/oracle/oracle-original.svg](https://github.com/devicons/devicon/blob/7330accdbc47e2dc0c19789a48533c4a3c50fe58/icons/oracle/oracle-original.svg) | [MIT](https://github.com/devicons/devicon/blob/7330accdbc47e2dc0c19789a48533c4a3c50fe58/LICENSE) |
| Firebird | `firebird.svg` | [devicon/icons/firebird/firebird-plain.svg](https://github.com/devicons/devicon/blob/7330accdbc47e2dc0c19789a48533c4a3c50fe58/icons/firebird/firebird-plain.svg) | [MIT](https://github.com/devicons/devicon/blob/7330accdbc47e2dc0c19789a48533c4a3c50fe58/LICENSE) |
| CockroachDB | `cockroachlabs.svg` | [simple-icons/icons/cockroachlabs.svg](https://github.com/simple-icons/simple-icons/blob/5d5d4d1d28cbb00b21770bb69d8112da52211a95/icons/cockroachlabs.svg) | [CC0 1.0](https://github.com/simple-icons/simple-icons/blob/5d5d4d1d28cbb00b21770bb69d8112da52211a95/LICENSE.md) |
| Cloud Spanner | `cloud-spanner.svg` | [simple-icons/icons/googlecloudspanner.svg](https://github.com/simple-icons/simple-icons/blob/5d5d4d1d28cbb00b21770bb69d8112da52211a95/icons/googlecloudspanner.svg) | [CC0 1.0](https://github.com/simple-icons/simple-icons/blob/5d5d4d1d28cbb00b21770bb69d8112da52211a95/LICENSE.md) |

The vendored monochrome marks (`mysql.svg`, `mariadb.svg`, `cockroachlabs.svg`)
were near-white for the dark surface; they are filled with each vendor's brand
color instead (`#4479A1`, `#C0765A`, `#7B52FF`) so they read on both the Dark
and the Light theme.

`clickhouse.svg` is an original four-bar mark drawn for this app (no vendor
artwork copied); it uses ClickHouse's yellow `#FFCC01` so it reads on the dark
surface.

The CockroachDB entry uses the Cockroach Labs mark because that is the
vendor-published icon available in the checked public collection. DB2,
Informix, JDBCX, IRIS, InterSystems Caché, and Dolt remain monogram fallbacks:
no exact, clearly licensed product SVG was verified for this picker.

`PostgreSQL` and `Custom (PostgreSQL)` reuse the existing local
`postgres.svg`; the custom MySQL entry reuses `mysql.svg`. Availability remains
driven solely by `ConnectionKind::is_supported()`.

## Sidebar tree glyphs

`database.svg`, `table.svg`, `eye.svg`, `refresh-cw.svg`, `locate-fixed.svg`,
`folder-plus.svg`, `upload.svg`, `download.svg`, `chevrons-down-up.svg`,
`arrow-down-up.svg`, `list-ordered.svg`, `arrow-down-a-z.svg`, `arrow-up-z-a.svg`, `plug.svg`,
`folder-input.svg`, `trash.svg`, `list-checks.svg`, `file-play.svg`, and
`chevrons-left.svg` are copied verbatim from the pinned
`gpui-kit-assets 0.6.6` dependency's bundled Lucide set
(`~/.cargo/registry/src/.../gpui-kit-assets-0.6.6/assets/icons/`).
Lucide is [ISC licensed](https://github.com/lucide/lucide/blob/main/LICENSE);
see `LICENSE-LUCIDE` in the `gpui-kit-assets` package. They are
`stroke="currentColor"` outlines tinted at render time with the
per-kind row icon colors.

## Result grid cell context-menu glyphs

`eraser.svg`, `arrow-up.svg`, `arrow-down.svg`, `columns-3.svg`,
`rows-3.svg`, `circle-x.svg`, `wand-sparkles.svg`, `type.svg`,
`calendar-clock.svg`, `calendar-days.svg`, `key-round.svg`, `hash.svg`,
`snowflake.svg`, `camera.svg`, `circle-plus.svg`, `square-dashed.svg`, and
`arrow-left-right.svg` are copied verbatim from the same pinned
`gpui-kit-assets 0.6.6` Lucide set for the cell context menu
(connected-table sort, page sort, details, copy/scopes/DSV, generate,
transpose, screenshot preflight, clone/delete, export, refresh). Disabled
menu entries name their blocker in footer labels; no fake capability.

`pin.svg`, `minimize-2.svg`, `maximize-2.svg` and `crosshair.svg` are copied
verbatim from the same pinned `gpui-kit-assets 0.6.6` Lucide set for the
workspace tab bar (pinned-tab glyph, Shorten/Show Full Tab Titles, Locate in
sidebar context-menu entries).

## SQL filter builder glyphs

`eye-off.svg` and `grip-vertical.svg` (rule enable/disable toggle and the
drag-reorder handle of the SQL table filter builder) are copied verbatim from
the pinned `gpui-kit-assets 0.7.0` Lucide set
(`~/.cargo/registry/src/.../gpui-kit-assets-0.7.0/assets/icons/`), ISC licensed
like the glyphs above. `eye.svg` and `filter.svg` are reused.

## MongoDB sidebar menu glyphs

`copy-plus.svg` (View in New Tab / Clone Collection), `pencil-ruler.svg`
(Manage Indexes) and `table-properties.svg` (View Data) are copied verbatim
from the pinned `gpui-kit-assets 0.7.0` Lucide set, ISC licensed like the
glyphs above. They label the same MongoDB menu entries.

## Error Log glyphs

`triangle-alert.svg` (status-bar icon, warning rows) and `info.svg` (info rows)
are copied verbatim from the pinned `gpui-kit-assets 0.6.6` Lucide set, ISC
licensed like the glyphs above. `circle-x.svg`, `copy.svg`, `download.svg`,
`folder-open.svg`, `eraser.svg` and `trash.svg` are reused.

## ER diagram glyph

`network.svg` (rail "Diagram" button) is copied verbatim from the pinned
`gpui-kit-assets 0.7.1` Lucide set, ISC licensed like the glyphs above.

`ellipsis.svg` (the diagram toolbar's overflow menu) is copied verbatim from the pinned
`gpui-kit-assets 0.7.1` Lucide set, ISC licensed like the glyphs above.

## Editor search panel glyphs

`case-sensitive.svg`, `replace.svg` and `chevron-left.svg` are copied verbatim from
the pinned `gpui-kit-assets 0.8.0` Lucide set (ISC, `LICENSE-LUCIDE` in that
package). gpui-component draws them by path in the code editor's built-in
find / replace panel (`IconName::CaseSensitive`, `Replace`, `ChevronLeft`); without
them the panel's case, replace-mode and previous-match buttons are empty squares.

## Result chart glyph

`chart-column-big.svg` (rail "Chart" button of the result pane) is copied verbatim from the pinned
`gpui-kit-assets 0.8.0` Lucide set, ISC licensed like the glyphs above.
