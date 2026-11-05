//! The review dialog of a structure edit: every pending change with the SQL it becomes, the
//! warnings, Copy SQL, Discard (one / all) and Apply.
//!
//! The builder reads the host on every render, so discarding a change or a finished apply shows
//! in place. Apply and Cancel are the host's (`ReviewHost`): it checks, emits the request to
//! AppRoot and records the answer in its [`EditSession`].

use std::cell::RefCell;
use std::rc::Rc;

use super::super::*;
use super::session::{ReviewModel, ReviewSnapshot};
use super::session::Wording;
use super::{EditContext, EditSession, SessionEdit};
use crate::sql_ddl::{Change, Planned, Severity, Warning};
use gpui::ClipboardItem;
use gpui_component::scroll::ScrollableMask;
use gpui_component::ActiveTheme;

/// The most the change list is tall; it scrolls beyond that.
const LIST_HEIGHT: f32 = 360.0;
/// From this many changes the list is virtual (a gpui `list` of a fixed height). Its height is not
/// taken from its cards (gpui measures those at their widest, where no text wraps), so it must be
/// full whatever the cards are: this many cards of at least [`MIN_CARD_HEIGHT`] fill it. Below, the
/// cards are plain children of a scrolling div (at most this many are laid out).
pub(in crate::app) const VIRTUAL_FROM: usize = (LIST_HEIGHT / MIN_CARD_HEIGHT) as usize + 1;
/// The height of the shortest card (a title row, one line of text, the padding and the rule);
/// the `a_card_is_never_shorter_than_the_virtual_list_assumes` test measures the real ones.
pub(in crate::app) const MIN_CARD_HEIGHT: f32 = 60.0;
/// How far past the edge of the list its cards are measured, so they do not pop in while scrolling.
const LIST_OVERDRAW: f32 = 240.0;

/// What decides the height of a card in the change list: when it differs from the one the list
/// measured its cards for, the list measures them again.
struct CardsKey {
    planned: Arc<Planned>,
    warnings: Arc<Vec<Warning>>,
    stale: Arc<Vec<Option<String>>>,
}

impl CardsKey {
    fn is(&self, model: &ReviewModel) -> bool {
        Arc::ptr_eq(&self.planned, &model.planned) && Arc::ptr_eq(&self.warnings, &model.warnings) && Arc::ptr_eq(&self.stale, &model.stale)
    }
}

/// Tells the list how many cards there are and that they may have changed height, when the plan,
/// the warnings or the stale marks are not what the cards were measured for. The scroll position stays.
fn sync_cards(list: &gpui::ListState, measured: &RefCell<Option<CardsKey>>, model: &ReviewModel) {
    if measured.borrow().as_ref().is_some_and(|key| key.is(model)) {
        return;
    }
    let (old, new) = (list.item_count(), model.planned.entries.len());
    if new > old {
        list.splice(old..old, new - old);
    } else if new < old {
        list.splice(new..old, 0);
    }
    list.remeasure_items(0..new);
    *measured.borrow_mut() = Some(CardsKey { planned: Arc::clone(&model.planned), warnings: Arc::clone(&model.warnings), stale: Arc::clone(&model.stale) });
}

/// A checkbox the host adds to the review (CASCADE of a drop, RESTART IDENTITY of a truncate): a
/// choice that changes the SQL, so the SQL shown is the SQL of the box as ticked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct ReviewOption {
    pub(in crate::app) key: &'static str,
    pub(in crate::app) label: String,
    pub(in crate::app) checked: bool,
}

/// What the review dialog needs from the entity it edits for.
pub(in crate::app) trait ReviewHost: Sized + 'static {
    /// The host's undoable steps (see [`SessionEdit`]).
    type Edit: SessionEdit;
    /// How the dialog speaks of the place it was opened from.
    const WORDING: Wording;
    fn session(&self) -> &EditSession<Self::Edit>;
    fn session_mut(&mut self) -> &mut EditSession<Self::Edit>;
    /// `None`: this host cannot edit structure at all (no dialog).
    fn edit_context(&self) -> Option<&EditContext>;
    /// The dialog is about to open: bring the host's own pending input into the draft.
    fn before_review(&mut self, cx: &mut Context<Self>);
    fn discard_change(&mut self, index: usize, cx: &mut Context<Self>);
    fn discard_all_changes(&mut self, cx: &mut Context<Self>);
    /// Apply was pressed on the change-set that was reviewed, with the SQL that was shown.
    fn request_apply(&mut self, reviewed: Vec<Change>, script: String, cx: &mut Context<Self>);
    /// The Cancel button of the running apply.
    fn cancel_apply(&mut self, cx: &mut Context<Self>);
    /// A short message for the host's own status line.
    fn show_notice(&mut self, text: String, failed: bool, cx: &mut Context<Self>);
    /// The boxes under the changes (none for a host whose SQL has no variants).
    fn options(&self) -> Vec<ReviewOption> {
        Vec::new()
    }
    /// A box of [`Self::options`] was ticked or cleared: the host edits its draft to match.
    fn set_option(&mut self, _key: &'static str, _on: bool, _cx: &mut Context<Self>) {}
    /// Something the open dialog keeps alive for as long as it is open: when the dialog closes (Close, Escape, an apply that
    /// succeeded) it is dropped. A host whose draft holds a password hands the guard that wipes it.
    fn dialog_guard(&self) -> Option<Rc<dyn std::any::Any>> {
        None
    }
}

/// Closes the dialog in `window` once the current update has finished.
pub(in crate::app) fn close_dialog_later<V: 'static>(window: gpui::AnyWindowHandle, cx: &mut Context<V>) {
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, cx| window.close_dialog(cx));
    });
}

/// Opens the review dialog of `host`; `false` when it could not be opened (no editing context, another dialog open).
pub(in crate::app) fn open_review<V: ReviewHost>(host: &mut V, window: &mut Window, cx: &mut Context<V>) -> bool {
    if host.edit_context().is_none() || window.has_active_dialog(cx) {
        return false;
    }
    host.before_review(cx);
    host.session_mut().begin_review(window.window_handle());
    let guard = host.dialog_guard();
    let view = cx.entity();
    // A long change list is virtual (see `VIRTUAL_FROM`): only the cards in view (and a little past it) are built.
    let list = gpui::ListState::new(0, gpui::ListAlignment::Top, px(LIST_OVERDRAW));
    let measured: RefCell<Option<CardsKey>> = RefCell::new(None);
    // Where a short (plain) change list is scrolled to; the dialog is rebuilt on every render.
    let plain_scroll = gpui::ScrollHandle::new();
    window.open_dialog(cx, move |dialog, _, cx| {
        // Held for as long as the dialog is (see `ReviewHost::dialog_guard`).
        let _guard = &guard;
        let Some((model, review, mono, options)) = view.read_with(cx, |this, cx| {
            let model = this.edit_context().map(|context| this.session().review_model(context))?;
            Some((model, this.session().review_snapshot(), cx.theme().mono_font_family.clone(), this.options()))
        }) else {
            return dialog;
        };
        let model = Rc::new(model);
        if model.planned.entries.len() >= VIRTUAL_FROM {
            sync_cards(&list, &measured, &model);
        }
        let count = model.changes().len();
        let applying = review.applying;
        let blocked = model.disabled.is_some() || count == 0 || !model.planned.executable() || applying || model.any_stale();
        let ready = !blocked && (!model.destructive() || review.understood) && (!model.needs_confirm || review.confirmed);
        let reviewed = (Arc::clone(&model.draft), model.script.clone());
        let copied = model.script.clone();
        let body = body(&view, &model, &review, &options, mono, V::WORDING, (&list, &plain_scroll));
        let (apply_view, discard_view, copy_view, cancel_view) = (view.clone(), view.clone(), view.clone(), view.clone());
        let cancelling = review.cancelling;
        dialog
            .w(px(720.0))
            .title(if count == 0 { "Changes".to_string() } else { format!("Changes ({count})") })
            .child(body)
            .footer(
                gpui_component::dialog::DialogFooter::new()
                    .child(
                        Button::new("changes-copy")
                            .label("Copy SQL")
                            .compact()
                            .ghost()
                            .disabled(copied.is_empty())
                            .debug_selector(|| "changes-copy".into())
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copied.to_string()));
                                copy_view.update(cx, |this, cx| this.show_notice("SQL copied to the clipboard".into(), false, cx));
                            }),
                    )
                    .child(
                        Button::new("changes-discard-all")
                            .label("Discard all")
                            .compact()
                            .ghost()
                            .disabled(count == 0 || applying)
                            .debug_selector(|| "changes-discard-all".into())
                            .on_click(move |_, _, cx| {
                                discard_view.update(cx, |this, cx| this.discard_all_changes(cx));
                            }),
                    )
                    .children(applying.then(|| {
                        Button::new("changes-cancel")
                            .label(if cancelling { "Cancelling..." } else { "Cancel" })
                            .compact()
                            .ghost()
                            .disabled(cancelling)
                            .debug_selector(|| "changes-cancel".into())
                            .on_click(move |_, _, cx| {
                                cancel_view.update(cx, |this, cx| this.cancel_apply(cx));
                            })
                    }))
                    .child(
                        Button::new("changes-close")
                            .label("Close")
                            .compact()
                            .ghost()
                            .debug_selector(|| "changes-close".into())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("changes-apply")
                            .label(if applying { "Applying..." } else { "Apply" })
                            .compact()
                            .primary()
                            .disabled(!ready)
                            .debug_selector(|| "changes-apply".into())
                            .on_click(move |_, _, cx| {
                                let (draft, script) = &reviewed;
                                let (changes, script) = (draft.changes.clone(), script.to_string());
                                apply_view.update(cx, |this, cx| this.request_apply(changes, script, cx));
                            }),
                    ),
            )
    });
    cx.notify();
    true
}

fn muted(text: impl Into<gpui::SharedString>) -> Div {
    div().text_size(px(11.0)).text_color(theme::text_muted()).child(text.into())
}

fn body<V: ReviewHost>(
    view: &gpui::Entity<V>,
    model: &Rc<ReviewModel>,
    review: &ReviewSnapshot,
    options: &[ReviewOption],
    mono: gpui::SharedString,
    wording: Wording,
    (list, plain_scroll): (&gpui::ListState, &gpui::ScrollHandle),
) -> gpui::AnyElement {
    let header = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(div().debug_selector(|| "changes-subject".into()).text_size(px(12.0)).text_color(theme::text()).child(format!("On {}", model.subject)))
        .child(
            div().debug_selector(|| "changes-mode".into()).child(muted(if model.transactional {
                format!("{} runs the statements in one transaction: all of them apply, or none.", model.label)
            } else if model.ddl_transactional {
                format!(
                    "{} cannot run CREATE DATABASE inside a transaction: the statements run one by one and stop at the first error; what ran stays applied.",
                    model.label
                )
            } else {
                format!(
                    "{} cannot roll DDL back: the statements run one by one and stop at the first error; what ran stays applied.",
                    model.label
                )
            })),
        );
    let banner = model.disabled.clone().map(|reason| {
        div()
            .debug_selector(|| "changes-disabled".into())
            .p(px(8.0))
            .rounded(px(4.0))
            .bg(theme::pending_cell())
            .text_size(px(11.5))
            .text_color(theme::text())
            .child(reason)
    });
    let count = model.planned.entries.len();
    // The body of the dialog is a `Scrollable`, whose `ScrollBounce` takes every wheel event over
    // it while the dialog is taller than the window. Either list therefore has a
    // `ScrollableMask` beside it: that scrolls the list first and hands the wheel back to the
    // dialog once the list is at its end.
    let cards: gpui::AnyElement = if count < VIRTUAL_FROM {
        let (view, model, applying) = (view.clone(), Rc::clone(model), review.applying);
        div()
            .debug_selector(|| "changes-list".into())
            .relative()
            .child(
                div()
                    .id("changes-list")
                    .max_h(px(LIST_HEIGHT))
                    .overflow_y_scroll()
                    .track_scroll(plain_scroll)
                    .flex()
                    .flex_col()
                    .children((0..count).map(|entry| card(&view, &model, applying, &mono, entry))),
            )
            .child(ScrollableMask::new(gpui::Axis::Vertical, plain_scroll))
            .into_any_element()
    } else {
        let (view, model, applying) = (view.clone(), Rc::clone(model), review.applying);
        let cards = gpui::list(list.clone(), move |entry, _, _| card(&view, &model, applying, &mono, entry))
            .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
            .w_full()
            .h(px(LIST_HEIGHT));
        div()
            .debug_selector(|| "changes-list".into())
            .relative()
            .child(cards)
            .child(ScrollableMask::new(gpui::Axis::Vertical, list))
            .into_any_element()
    };
    let confirmations = {
        let understood_view = view.clone();
        let confirmed_view = view.clone();
        let applying = review.applying;
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(options.iter().map(|option| {
                let (option_view, key) = (view.clone(), option.key);
                div().debug_selector(move || format!("changes-option-{key}")).child(
                    Checkbox::new(gpui::SharedString::from(format!("changes-option-{key}-box")))
                        .checked(option.checked)
                        .disabled(applying)
                        .label(option.label.clone())
                        .on_click(move |checked: &bool, _, cx| {
                            let checked = *checked;
                            option_view.update(cx, |this, cx| this.set_option(key, checked, cx));
                        }),
                )
            }))
            .children(model.destructive().then(|| {
                div().debug_selector(|| "changes-understood".into()).child(
                    Checkbox::new("changes-understood-box")
                        .checked(review.understood)
                        .label("I understand that data will be lost")
                        .on_click(move |checked: &bool, _, cx| {
                            let checked = *checked;
                            understood_view.update(cx, |this, cx| {
                                this.session_mut().set_understood(checked);
                                cx.notify();
                            });
                        }),
                )
            }))
            .children(model.needs_confirm.then(|| {
                div().debug_selector(|| "changes-confirm".into()).child(
                    Checkbox::new("changes-confirm-box")
                        .checked(review.confirmed)
                        .label(format!("Run {} on {}", pluralize(model.statements()), model.subject))
                        .on_click(move |checked: &bool, _, cx| {
                            let checked = *checked;
                            confirmed_view.update(cx, |this, cx| {
                                this.session_mut().set_confirmed(checked);
                                cx.notify();
                            });
                        }),
                )
            }))
    };
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(header)
        .children(review.applying.then(|| {
            div()
                .debug_selector(|| "changes-progress".into())
                .p(px(8.0))
                .rounded(px(4.0))
                .bg(theme::grid_header())
                .text_size(px(11.5))
                .text_color(theme::text())
                .child(if review.cancelling {
                    "Cancelling: the running statement is being stopped...".to_string()
                } else {
                    format!(
                        "Applying {} on a separate session of the connection (your own transaction is not touched). Cancel stops it{}.",
                        pluralize(model.statements()),
                        if model.transactional { " and rolls everything back" } else { "; what already ran stays applied" }
                    )
                })
        }))
        .children(banner)
        .child(if model.planned.entries.is_empty() {
            div()
                .debug_selector(|| "changes-empty".into())
                .py(px(12.0))
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(wording.empty)
                .into_any_element()
        } else {
            cards
        })
        .child(confirmations)
        .children(review.report.clone().map(|(text, failed)| {
            div()
                .debug_selector(|| "changes-report".into())
                .max_h(px(140.0))
                .overflow_hidden()
                .p(px(8.0))
                .rounded(px(4.0))
                .bg(if failed { gpui::Rgba { a: 0.14, ..theme::danger() } } else { theme::grid_header() })
                .text_size(px(11.5))
                .text_color(if failed { theme::danger() } else { theme::text() })
                .child(text)
        }))
        .into_any_element()
}

/// Card `entry` of the change list: a change with its warnings, its SQL and Discard.
fn card<V: ReviewHost>(
    view: &gpui::Entity<V>,
    model: &ReviewModel,
    applying: bool,
    mono: &gpui::SharedString,
    entry: usize,
) -> gpui::AnyElement {
    let entry = &model.planned.entries[entry];
    let change = entry.change;
    let discard = view.clone();
    let own: Vec<&Warning> = model.warnings.iter().filter(|w| w.change == change).collect();
    let statements: Vec<gpui::AnyElement> = match &entry.statements {
        Ok(statements) => statements
            .iter()
            .enumerate()
            .map(|(k, sql)| {
                div()
                    .debug_selector(move || format!("changes-sql-{change}-{k}"))
                    .w_full()
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(3.0))
                    .bg(theme::grid_header())
                    .font_family(mono.clone())
                    .text_size(px(11.0))
                    .text_color(theme::text())
                    .child(sql.clone())
                    .into_any_element()
            })
            .collect(),
        Err(error) => vec![div()
            .debug_selector(move || format!("changes-error-{change}"))
            .text_size(px(11.5))
            .text_color(theme::danger())
            .child(error.to_string())
            .into_any_element()],
    };
    let stale_reason = model.stale.get(change).cloned().flatten().map(|reason| {
        div()
            .debug_selector(move || format!("changes-stale-{change}"))
            .text_size(px(11.5))
            .text_color(theme::danger())
            .child(format!("Stale: {reason}"))
    });
    let folded = entry.folded_into.map(|host| muted(format!("Part of the CREATE TABLE statement of change {}.", host + 1)));
    div()
        .debug_selector(move || format!("changes-item-{change}"))
        .w_full()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .py(px(6.0))
        .border_b_1()
        .border_color(theme::divider())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.5))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(model.changes()[change].title()),
                )
                .child(
                    Button::new(("changes-discard", change))
                        .label("Discard")
                        .compact()
                        .ghost()
                        .disabled(applying)
                        .debug_selector(move || format!("changes-discard-{change}"))
                        .on_click(move |_, _, cx| {
                            discard.update(cx, |this, cx| this.discard_change(change, cx));
                        }),
                ),
        )
        .children(stale_reason)
        .children(own.into_iter().map(|warning| {
            div()
                .debug_selector(move || format!("changes-warning-{change}"))
                .flex()
                .items_start()
                .gap(px(4.0))
                .text_size(px(11.5))
                .text_color(if warning.severity == Severity::Destructive { theme::danger() } else { theme::text_muted() })
                .child(
                    Icon::empty()
                        .path("icons/triangle-alert.svg")
                        .size(px(12.0))
                        .flex_shrink_0()
                        .text_color(if warning.severity == Severity::Destructive { theme::danger() } else { theme::warning() }),
                )
                .child(div().flex_1().min_w_0().child(warning.message.clone()))
        }))
        .children(folded)
        .children(statements)
        .into_any_element()
}

fn pluralize(count: usize) -> String {
    format!("{count} {}", if count == 1 { "statement" } else { "statements" })
}
