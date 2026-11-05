# TableX

Performance-first native macOS database client with multi-DB extensibility. Performance is #1 — always optimize for speed, low memory usage, and responsiveness.

## Core Principles

- **Performance first** — analyze perf trade-offs before every coding decision. Prefer zero-copy, lazy evaluation, async patterns, and minimal allocations.
- **Multi-DB extensibility** — every feature should support adding new database types (PostgreSQL, MySQL, SQLite, Redis, etc.), not just MongoDB. Design abstractions and interfaces to be database-agnostic.
- No unnecessary abstractions that add overhead.

## Language-Specific

### Rust (`rust-engine/`)
When working with Rust code, consult these skills:
- `.agents/skills/rust-engineer/`
- `.agents/skills/rust-patterns/`
- `.agents/skills/rust-async-patterns/`
- `.agents/skills/rust-testing/`

## Reuse Before Custom Code

- Before implementing behavior, check in this order:
  1. Existing project implementation.
  2. Standard library or native platform API.
  3. Official components/APIs in installed dependencies.
  4. Custom implementation only when the above cannot meet a concrete requirement.
- Verify candidates against the pinned dependency source. Do not assume an API is unavailable because it was absent from an example, skill, or previous search.
- Before writing a custom replacement, identify the existing component considered and the exact missing capability. "More control", "fewer entities", or "simpler integration" alone are not sufficient reasons.
- A composition problem is not proof that a component is unsuitable. First check whether the surrounding layout, ownership, focus, or overlay structure is causing the limitation.
- Do not silently replace an agreed standard component with a custom alternative. Explain the concrete blocker and obtain approval before changing the approach.
- Prefer removing custom code over maintaining a parallel implementation of behavior already provided by a dependency.

## Debugging

When encountering any bug, test failure, crash, or unexpected behavior — analyze root cause before proposing any fix. Apply systematic debugging workflow.

## Git Policy

- ALWAYS ask for explicit permission before making any git commit. Never commit without user approval.
- ALWAYS ask for explicit permission before running git push. Never push without user approval.

## Commit Messages

The `area: summary` style of Zed, Tokio and the Linux kernel.

- Subject: `<area>: <summary>`, 72 characters at most. The area is the component changed, in lowercase: a crate's short name (`app`, `mcp`, `driver-kit`, `driver-host`, `sqlite`, `postgres`, `mysql`, `mongo`) or `ci`, `deps`, `docs`, `scripts`. Two areas: `mysql, mariadb:`. A change across many names the one it is about.
- Summary: imperative mood ("add", "fix", "keep"), lowercase start, no trailing period. Say what changes, not how.
- Body: none by default. Only when the reason is not obvious from the subject (a bug's cause, a trade-off): a blank line, then at most 3 lines wrapped at 72.
- `[skip ci]` goes on the last line of the message, not in the subject: `git commit -m "<subject>" -m "[skip ci]"`. Leave it out when the change needs CI (CI config, cross-platform fixes, dependency updates).
- One logical change per commit; dependency updates get their own `deps:` commit.

Examples: `driver-kit: keep a cancel that arrives before its operation registers`, `deps: update rustls to 0.23.46`, `ci: install fish for the shell quoting test`.
