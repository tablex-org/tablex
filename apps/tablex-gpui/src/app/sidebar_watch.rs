//! What keeps the cached left pane (`navigator_ui::SidebarView`) from going stale.
//!
//! The pane replays its last frame unless it is notified, so every change to what it draws has to
//! reach it. That used to be a convention (call `notify_chrome` or `cx.notify()`, not `notify_tab`);
//! this module makes it structural: the state the pane draws is held in [`Watched`] fields of
//! `AppRoot`. A `&mut` borrow of one (a field write, a `&mut self` method of the model) goes through
//! `DerefMut`, which moves the app's [`SidebarEpoch`]. `AppRoot::sync_sidebar` compares the epoch
//! with the one the pane was last notified at (it runs on every tab, chrome and root notification;
//! the root's render compares it with the one the pane last drew at and embeds the pane uncached when it
//! is behind), so a path that changes the tree and then notifies only a tab, or only a different view,
//! still brings the pane up to date. There is no way to get a `&mut` to the state without moving
//! the epoch, so forgetting is a compile error for new code, not a review comment.
//!
//! Why this and not accessors that take `cx` or a guard that notifies on drop: the model is mutated
//! by hundreds of `&mut self` calls, many in code that has no `cx` (the model's own methods, load
//! results applied under another entity); a guard that needs a `cx` could not be handed out there.
//! The epoch costs one `Cell` increment per mutable borrow and one integer compare per root render
//! or tab notification: no per-row work, no allocation, no entity, no listener. A mutable borrow
//! that changes nothing costs the pane one extra render, which is the safe direction.
//!
//! # What the pane draws (the inventory; keep it in step with `SIDEBAR_WATCHED` in `tests/sidebar_view.rs`, which a test checks against `AppRoot`)
//!
//! Held in [`Watched`] fields of `AppRoot`:
//!
//! * `navigator` (`NavigatorState`): the tree (visible rows, loaded children, per-node load state
//!   and errors, labels, kinds, provider nodes, driver kinds), expansion and the remembered tree,
//!   selection and its anchor, the search options and matchers, folder filters and the visible
//!   filters, pins, the Bookmarks / group / driver-section collapse state, the connection list and
//!   its groups, folders and projects, and the sidebar preferences it owns (group names and
//!   colors, hidden connections, sort, widths).
//! * `sidebar` (`SidebarState`): the error, notice, search-error and locate-error lines, the
//!   remote-search progress, the per-connection colors and the overrides of them, the pin store.
//!   (Its focus handle and scroll handle are the pane's own GPUI handles: their changes reach the
//!   pane through GPUI. The search field is `AppRoot::search_field`, outside the watch so that the
//!   render that builds it does not mark the pane as changed.)
//! * `navigator_ui` (`NavigatorUiState`): the drag-over drop target, the open row-problem popover.
//! * `saved_connections`: the connection records the rows and the settings menu read.
//! * `connection_policy`: each connection's type (the badge and the tint of its row).
//! * `settings`: `navigation.show_table_stats` and the connection types.
//! * `table_stats`: the row and size estimates shown at a table row's edge.
//! * `tunnel_ui`: the live tunnels (the SSH / Proxy badge of a connection).
//!
//! Tracked by their own revision (a finer one, kept as it was): `sessions`, the connection status
//! dots, the live set and the connected-only dot (`ConnectionSessions::revision`).
//!
//! Notified by GPUI itself, not by this module: a row's hover, the list's scroll, the search
//! field's text and focus, the window's refresh (theme, language) and a change of the pane's
//! bounds (the sidebar's width, the window's height). The context menus, the rename and the
//! delete dialogs are separate entities, drawn above the pane, not by it.
//!
//! Not read by the pane, so not watched: the schema-columns store and the folder describes (their
//! answers reach the tree as navigator loads), the workspace and its tabs (the pane does not mark
//! the active tab; "locate" is an action), the editors, the grids and the shell's other chrome.

use std::cell::Cell;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

/// Counts the mutable borrows of the state the left pane draws. One per `AppRoot`, shared by all of
/// its [`Watched`] fields.
#[derive(Clone, Default)]
pub(super) struct SidebarEpoch(Rc<Cell<u64>>);

impl SidebarEpoch {
    /// Where the state is now; the pane is current when it was notified at this value.
    pub(super) fn now(&self) -> u64 {
        self.0.get()
    }

    /// Puts `value` under this epoch: every `&mut` borrow of it moves the epoch.
    pub(super) fn watch<T>(&self, value: T) -> Watched<T> {
        Watched {
            value,
            epoch: self.clone(),
        }
    }
}

/// State the left pane draws (see the module docs). Reading is free (`Deref`); a `&mut` borrow
/// (`DerefMut`: a field write, a `&mut self` method) moves the epoch.
pub(super) struct Watched<T> {
    value: T,
    epoch: SidebarEpoch,
}

impl<T> Deref for Watched<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Watched<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.epoch.0.set(self.epoch.0.get().wrapping_add(1));
        &mut self.value
    }
}
