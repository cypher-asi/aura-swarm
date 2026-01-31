# rules.md — Rust Code Quality & Conventions (Cursor)

This document defines how we write, format, test, lint, and ship Rust code in this repo. Follow it strictly.

---

## 0) Non-negotiables

- Code must compile in CI with **no warnings**.
- Formatting must be clean (rustfmt).
- Linting must be clean (Clippy, warnings denied).
- Tests must pass (unit + integration).
- No unsafe Rust unless explicitly approved, isolated, and tested.
- External side effects must be behind explicit boundaries (e.g., tools/executors), never implicit.

---

## 1) Project setup (local + CI)

### 1.1 Required installs

Install Rust via rustup, then ensure these are available:

- `cargo`
- `rustfmt` component
- `clippy` component

Install rustup components:

- rustfmt: `rustup component add rustfmt`
- clippy: `rustup component add clippy`

Recommended tools (strongly encouraged):

- `cargo-deny` (dependency and license checks)
- `cargo-audit` (security advisories)
- `cargo-nextest` (faster test runner)
- `cargo-llvm-cov` (coverage)
- `cargo-udeps` or `cargo-machete` (unused dependency checks)

Install recommended tools:

- cargo-deny: `cargo install cargo-deny`
- cargo-audit: `cargo install cargo-audit`
- nextest: `cargo install cargo-nextest`
- llvm-cov: `cargo install cargo-llvm-cov`
- machete: `cargo install cargo-machete`

Optional (useful for formatting/lint in editor):

- `rust-analyzer` (via your editor/VSCode extension)

### 1.2 Rust version policy (MSRV)

- The repo must define a **minimum supported Rust version (MSRV)**.
- CI must enforce MSRV (build + test at MSRV).
- Avoid unstable features in production code.

Where to define:
- Put `rust-version = "X.Y.Z"` in workspace `Cargo.toml` (preferred).
- Use a `rust-toolchain.toml` to pin toolchain in dev/CI if desired.

### 1.3 Workspace conventions

- Prefer a Cargo workspace for multi-crate projects.
- Keep crates small and purpose-driven (single responsibility).
- Avoid cyclic dependencies between crates.

Example crate roles:
- `*-core`: IDs, schemas, pure logic, shared types
- `*-store`: persistence boundary
- `*-kernel`: deterministic logic and invariants
- `*-swarm`: orchestration/runtime
- `*-tools`: side-effect boundary (filesystem/commands/etc.)

### 1.4 CI gates (required)

CI must run these checks:

1) Format check  
- `cargo fmt --all -- --check`

2) Lint check (deny warnings)  
- `cargo clippy --all-targets --all-features -- -D warnings`

3) Tests  
- `cargo test --all --all-features`

Recommended CI checks (add when feasible):

4) Dependency advisories  
- `cargo audit`

5) Dependency policy + licenses  
- `cargo deny check`

6) Unused deps (optional, can be noisy)  
- `cargo machete`

7) Coverage (optional)  
- `cargo llvm-cov --all --all-features --lcov --output-path lcov.info`

### 1.5 Local pre-commit (recommended)

Developers should run this before pushing:

- `cargo fmt`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all --all-features`

If you use a pre-commit hook, it must be fast and deterministic.

---

## 2) Formatting and code style

### 2.1 rustfmt is authoritative

- Never hand-format.
- Run `cargo fmt` before commit.
- Keep formatting defaults; avoid exotic rustfmt options unless the team agrees.

### 2.2 Naming conventions

- `snake_case` for functions, modules, variables.
- `UpperCamelCase` for types and traits.
- `SCREAMING_SNAKE_CASE` for constants.
- Prefer descriptive names over abbreviations (except universally common: id, tx, db, cfg).

### 2.3 Idiomatic Rust

Prefer:
- `Result<T, E>` and `Option<T>` for error/absence
- `match` for clarity in multi-branch logic
- iterators when readable, loops when clearer

Avoid:
- deeply chained iterator pipelines that harm readability
- clever macros when plain Rust is clearer

### 2.4 Comments and docs

- Comments explain **why**, not what.
- Document invariants, assumptions, and failure modes.
- Delete stale comments; do not let them rot.

---

## 3) Public API discipline

- Default to `pub(crate)`.
- Keep public API surfaces small and stable.
- All public items must have docs unless self-evident.
- Do not expose lifetimes in public APIs unless necessary.

---

## 4) Error handling rules

### 4.1 No unwrap/expect in production code

`unwrap()` and `expect()` are allowed only in:
- tests
- examples
- unreachable code after validation, with a comment stating the invariant

### 4.2 Error types

- Use `thiserror` for library crates.
- Use `anyhow` only at application boundaries (bin crates, top-level command handlers).
- Errors must preserve context: include ids, keys, paths, seq numbers when relevant.

### 4.3 Context on fallible operations

- Add meaningful context at boundaries (IO, DB, network, tool calls).
- Do not swallow errors silently.
- If you intentionally ignore an error, document why.

---

## 5) Types, ownership, lifetimes

### 5.1 Strong domain types

- Use newtypes for identifiers and keys.
- Do not pass raw bytes around when a domain type exists.

Examples:
- `struct AgentId([u8; 32]);`
- `struct TxId([u8; 32]);`

### 5.2 Borrow by default

- Take `&T` or slices for inputs.
- Avoid cloning unless required; justify clones in hot paths.

### 5.3 Keep lifetimes simple

- Avoid exposing complex lifetimes in public APIs.
- If lifetimes become complex, consider owning buffers (Bytes, Arc, Vec) or redesign.

---

## 6) Async conventions (Tokio)

### 6.1 Never block the runtime

- Do not use blocking filesystem IO on async tasks.
- Do not run heavy CPU work on core async threads.
- Use `tokio::fs` or `tokio::task::spawn_blocking` as needed.

### 6.2 Timeouts at boundaries

- All external boundaries must have timeouts:
  - network calls
  - tool execution
  - IPC
- Fail fast with clear errors.

### 6.3 Cancellation safety

- Be mindful of cancellation in async code.
- Avoid leaving partial state; commits must remain atomic.

---

## 7) Testing standards

### 7.1 Required test layers

- Unit tests: pure functions, edge cases
- Integration tests: cross-module behavior, persistence, workflows
- Determinism tests: replay yields the same derived results

Recommended:
- Property tests for invariants (proptest)

### 7.2 What must be tested (minimum)

- Serialization round-trips for all persisted types
- Atomicity: commit is all-or-nothing under simulated failure
- Ordering: per-agent sequence correctness (no gaps, no duplicates)
- Concurrency: parallel agents do not violate single-writer per agent rule
- Tool sandbox: path traversal blocked; limits enforced

### 7.3 Test rules

- Tests must be deterministic (no flaky sleeps).
- Use `tempfile` for filesystem tests.
- Keep test names descriptive.

---

## 8) Linting and static checks

### 8.1 Clippy is required

- CI runs Clippy with warnings denied.
- Fix Clippy warnings rather than suppressing them.
- If you must allow a lint, scope it tightly and document why.

### 8.2 Warnings policy

- New warnings are not allowed.
- If a warning is introduced, it must be fixed before merge.

---

## 9) Dependency rules

- Prefer well-maintained crates with broad adoption.
- Avoid adding dependencies for small conveniences.
- Keep dependency trees lean.
- Document why security-sensitive dependencies exist.
- Regularly run advisory checks.

Recommended security hygiene:
- `cargo audit` in CI
- `cargo deny check` for licenses and bans

---

## 10) Performance and allocation discipline

- Avoid obvious waste in hot paths.
- Use capacity hints and reuse buffers where appropriate.
- Prefer immutable shared buffers for large data when beneficial (e.g., Bytes or Arc slices).
- Do not micro-optimize at the expense of clarity unless measured.

---

## 11) Security and safety rules

- No unsafe by default.
- Validate all external inputs.
- Enforce size limits on reads/writes and outputs.
- Do not log secrets or sensitive payloads.
- Filesystem access must be sandboxed and path-normalized to prevent traversal.

---

## 12) Logging and observability

- Use `tracing` for logs in runtime code.
- Include structured fields: agent_id, tx_id, seq, durations.
- Do not log large blobs by default; log hashes or summaries.
- Use log levels correctly: info for lifecycle, debug for details, warn for recoverable, error for failures.

---

## 13) Documentation requirements for critical modules

Any module that implements critical guarantees must document:
- invariants it enforces
- assumptions it relies on
- failure modes and recovery behavior

Critical examples:
- storage key formats
- atomic commit protocol
- replay semantics
- concurrency model
- tool sandbox rules

---

## 14) Code review checklist

- Format clean (fmt check passes)
- Clippy clean with warnings denied
- Tests pass locally and in CI
- No unwrap/expect in production paths
- Errors include context
- Public API minimized
- No blocking operations in async contexts
- Timeouts and limits enforced at boundaries
- Tool and filesystem operations are sandboxed
- No sensitive data in logs

---

## 15) Conventional Rust defaults (quick reference)

Prefer:
- `thiserror` for library errors
- `anyhow` at app boundaries
- newtypes for IDs
- `pub(crate)` by default
- deterministic replay semantics
- `tracing` for logs
- timeouts on external boundaries
- strict CI gates: fmt + clippy + test

Avoid:
- unwrap/expect in production
- hidden global state
- side effects outside explicit tool/executor boundaries
- nondeterministic tests
- blocking calls on async runtime threads

---
