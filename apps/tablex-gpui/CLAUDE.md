# TableX GPUI app

Loaded when working under `apps/tablex-gpui/`; the root `CLAUDE.md` still applies.

## GPUI Component Selection

- Use official `gpui-component` controls for standard interactions:
  - Single-value selection: `Select` / `SelectState`.
  - Button-triggered menus: `DropdownMenu` / `PopupMenu`.
  - Dropdown indicator: `Button::dropdown_caret`.
  - Text editing: `Input` / `InputState`.
- Do not recreate a dropdown using a column of buttons merely to avoid adapting its parent layout.
- Before combining overlays, inspect the pinned components' deferred drawing, anchoring, focus, dismissal, and event propagation behavior.
- Verify required icon paths are registered in the application's `AssetSource`. Referencing `IconName` does not guarantee that the icon exists.
- Use the project's theme integration for normal, hover, active, selected, disabled, and focus states. Avoid mixing default component colors with ad-hoc local overrides.
- Preserve standard keyboard access, Escape, click-outside dismissal, selection feedback, and disabled behavior. Do not drop them to simplify integration without explicit approval.

## Verification Requirements

- Tests must exercise the real interaction, not merely set model state or assert that the parent panel exists.
- For dropdowns, verify opening, visible options, selecting a different value, model update, dismissal, and reopening with the correct selection.
- For overlays, also verify trigger-relative placement, viewport edges, stacking, and absence of click-through.
- UI tests should initialize the production theme and asset source where applicable.
- Passing compilation and layout tests is not proof of visual correctness. State clearly when visual or interaction verification remains incomplete.
- Report known regressions and failed verification gates. Do not claim completion while silently omitting required behavior.

## GPUI Development

- **Performance first:** Before planning or implementing GPUI changes, load `.agents/skills/gpui-component-core/` and every applicable `gpui-component-*` skill.
- Verify proposed APIs against the pinned `gpui` and `gpui-component` source. Skill examples may target a different version; installed source is authoritative.
- Prefer official GPUI/gpui-component APIs and established project patterns. Do not add custom workarounds unless the pinned versions do not support the required behavior.
- If a workaround is unavoidable, document the unsupported capability, analyze its allocation and hot-path cost, and add a focused regression test or benchmark.
- Preserve virtualization. Do not add per-row/per-cell entities, listeners, timers, polling, or multiplicative scans without measurements proving the cost is acceptable.
- For performance-sensitive UI changes, verify allocation behavior and responsiveness before considering the task complete.
