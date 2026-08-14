# AGENTS.md

This file applies to the entire repository. More deeply nested `AGENTS.md` files, if added later,
override it for their subtree.

## Project Overview

ProviderX is a lightweight Rust egress router and Apple Silicon macOS menu-bar application for
Codex and ChatGPT Desktop. It keeps official OpenAI traffic transparent while routing namespaced
third-party models to configured providers. It supports OpenAI Responses, including WebSocket to
HTTP/SSE bridging, and OpenAI Chat Completions through protocol adapters.

The workspace uses Rust 2024, requires Rust 1.85 or newer, and forbids unsafe Rust. The primary
shipping target is Apple Silicon macOS. Keep `Cargo.lock` committed and update it intentionally
when dependencies change.

## Repository Map

- `crates/provider-x-core`: validated configuration, model and provider identities, routing,
  runtime snapshots, and proxy-environment policy. Keep this crate transport- and UI-neutral.
- `crates/provider-x-protocol`: protocol-neutral contracts for stateful WebSocket-to-HTTP bridges.
- `crates/protocol-openai-responses`: OpenAI Responses path handling, request inspection and
  rewriting, model-list behavior, HTTP/SSE framing, and WebSocket bridge state.
- `crates/protocol-openai-chat-completions`: conversion between Responses semantics and OpenAI
  Chat Completions HTTP/SSE semantics.
- `crates/provider-x-network`: shared direct/proxied HTTP, HTTPS, and WebSocket connector policy.
- `crates/provider-x-catalog`: provider model discovery, model-registry recommendations, review
  state, and private catalog projection.
- `crates/provider-x-egress`: loopback server, routing, authorization handling, connection limits,
  timeouts, streaming, cancellation, graceful shutdown, and runtime snapshot publication.
- `crates/provider-x-app`: control plane, secure persistence, Codex configuration integration,
  localization, GPUI settings UI, and the macOS tray/runtime lifecycle.
- `crates/provider-x-contract-probe`: executable used for controlled, redacted integration probes.
- `tests/contract`: real Codex/ChatGPT contract fixtures and experimental probes.
- `scripts`: macOS bundle build, verification, lifecycle smoke, and measurement scripts.

## Architectural Rules

- Organize routing and conversion by protocol, not by provider vendor. A vendor-specific UI
  template must compile into the same typed `ProviderConfig` used by custom providers.
- Treat a bare model ID as official traffic. Treat `<provider-id>/<upstream-model-id>` as managed
  third-party traffic, route by the provider namespace, and send only the upstream model ID to that
  provider. Unknown or stale namespaced models must fail closed.
- Preserve official request bodies, credentials, standard metadata, streaming behavior, and model
  IDs. For third-party traffic, replace credentials and rewrite only fields required by the selected
  protocol adapter.
- Keep the local listener bound to `127.0.0.1`. Do not weaken the per-install 256-bit ingress
  capability, exact capability matching, browser-origin rejection, request limits, connection
  limits, or timeout enforcement.
- Keep protocol state bounded. Preserve backpressure, cancellation propagation, terminal-event
  handling, and graceful shutdown behavior. Never automatically replay a Responses request that may
  already have reached an upstream.
- Build a complete validated runtime snapshot before publishing it. New requests may use the new
  snapshot; in-flight requests must retain the snapshot with which they started.
- Keep control-plane persistence and runtime publication consistent. Preserve the existing
  fingerprint checks, compare-and-swap writes, crash-recovery receipts, and fail-closed ordering.
- Respect `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` through `provider-x-network` and
  `provider-x-core`. Loopback destinations must continue to bypass external proxies.
- Keep protocol-specific parsing and conversion out of the generic network runner and egress
  orchestration layers. Extend the adapter traits when adding a protocol.

## Security and Privacy

- API keys, official authorization headers, cookies, OAuth tokens, account IDs, attestation data,
  ingress capabilities, complete request bodies, and original Codex configuration contents are
  secrets. Do not print, log, persist as test evidence, or include them in errors or debug output.
- New structs containing secrets must implement redacted `Debug` behavior. Add regression tests for
  redaction whenever a new secret-bearing type or diagnostic path is introduced.
- Preserve private application-storage permissions, regular-file checks, symlink and hard-link
  rejection, atomic replacement, and concurrent-change detection. Do not silently repair an unsafe
  existing directory or overwrite externally changed configuration.
- Do not read from or copy `~/.codex/auth.json`. Tests must use synthetic credentials unless the
  user explicitly authorizes a controlled live contract test.
- Bind test servers to loopback, use temporary homes and ports, clean up child processes, and verify
  that listeners are released. Never point an ordinary test at a production provider endpoint.
- Do not add secrets, real user paths, raw captures, or unredacted fixtures to the repository.

## Codex and ChatGPT Desktop Integration

- Limit managed Codex settings to the keys and receipt transaction implemented in
  `crates/provider-x-app/src/codex_config`. Preserve unrelated TOML, comments, exact original bytes,
  and external edits when enabling, reconciling, or restoring integration.
- Treat Codex's model cache invalidation and the install receipt as one recoverable transaction.
  Do not broaden file-permission acceptance or bypass configuration-drift checks.
- Provider or integration changes require a complete ChatGPT Desktop restart before the model
  picker is expected to refresh. Do not promise hot refresh from an app-server process or a cache
  rewrite alone.
- During integration disablement, keep the router alive long enough for existing tasks to finish.
  Drain first; force-close only after the configured grace period.

## macOS Application and Localization

- Keep the app an accessory/menu-bar application. Closing the settings window must not terminate
  the router, and reopening it must reuse the single running instance.
- Preserve single-instance locking and startup handoff behavior. The process lock must outlive the
  egress handle and Tokio runtime during shutdown.
- Keep platform-specific code under `crates/provider-x-app/src/platform/macos` or behind an
  appropriate `cfg(target_os = "macos")` boundary.
- User-visible strings belong in both `crates/provider-x-app/locales/en.yml` and `zh-CN.yml`; do not
  hard-code new UI copy in Rust. Keep `_version` and the full nested key set identical between the
  two locale files, and preserve interpolation variables on both sides.
- UI copy should describe an actionable user-visible effect, not internal cache, listener, or
  synchronization machinery.
- When changing icons, bundle metadata, signing, launch-at-login, tray behavior, or the GPUI shell,
  run the macOS bundle verification and lifecycle smoke tests described below.

## Development Workflow

Before editing:

1. Read the relevant crate manifest, public types, neighboring tests, and recent call sites.
2. Trace cross-crate changes from typed configuration through snapshot creation, routing, protocol
   conversion, and app publication rather than patching only the visible symptom.
3. Check `git status --short --branch` and preserve unrelated user changes.

While editing:

- Follow the workspace lints: `unsafe_code = "forbid"`, Clippy `all`, and Clippy `pedantic`.
- Use typed IDs and validated documents instead of passing unchecked strings between layers.
- Return structured errors with `thiserror`; use `anyhow` only at application orchestration
  boundaries where additional typing would not help callers.
- Keep async I/O non-blocking. Do not hold synchronous mutex guards across `.await` points.
- Add focused regression tests next to the affected crate. Prefer deterministic loopback fixtures
  over timing-only assertions or external services.
- Keep comments focused on invariants, failure ordering, safety, or non-obvious protocol behavior.
  Do not narrate straightforward code.
- Run `cargo fmt --all` after Rust changes. Do not edit generated files under `target/`.

## Validation

Use the smallest relevant checks during iteration, then widen validation according to the changed
surface.

Baseline repository checks:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Useful focused checks:

```sh
cargo test -p provider-x-core
cargo test -p provider-x-protocol
cargo test -p protocol-openai-responses
cargo test -p protocol-openai-chat-completions
cargo test -p provider-x-network
cargo test -p provider-x-catalog
cargo test -p provider-x-egress --test http_proxy
cargo test -p provider-x-app
cargo test -p provider-x-app --test codex_config
```

Apple Silicon macOS packaging and UI-shell checks:

```sh
./scripts/build-macos-app.sh
./scripts/create-macos-dmg.sh
./scripts/smoke-macos-shell.sh
```

`build-macos-app.sh` already invokes `verify-macos-app.sh` on the generated app. It creates an ad-hoc
signature by default; set `PROVIDER_X_CODESIGN_IDENTITY` only when an explicitly authorized signing
identity is required. `create-macos-dmg.sh` verifies that app bundle again before packaging it.
`measure-macos-shells.sh` is a measurement tool, not a pass/fail regression test; run it only for
performance or footprint work and report the observed environment.

Choose additional checks by risk:

- Routing, headers, auth, transports, limits, or cancellation: run the egress integration test and
  the affected protocol crate tests.
- Provider schema, model IDs, catalog projection, or refresh behavior: run core, catalog, control
  plane, and egress tests.
- Codex configuration or secure storage: run all `provider-x-app` tests, especially
  `codex_config`, `control_plane`, and `storage`.
- UI, tray, localization, launch-at-login, resources, or bundle metadata: run app tests, verify
  locale key parity, then run the macOS shell smoke.
- Workspace dependency or feature changes: run the complete baseline suite and the macOS build.

If a check cannot run because of the host, network, signing identity, GUI session, or external
service, state that limitation explicitly. Do not describe a unit test, handler test, or historical
result as live Codex, Desktop UI, or real-provider validation.

## Contract and Live Tests

- Ignored tests requiring credentials or live provider access are opt-in. Run them only when the
  user explicitly requests live validation and provides a safe credential mechanism; never expose
  the value in command output, logs, fixtures, or the final report.
- Real Codex and ChatGPT Desktop claims require current evidence from the actual executable and
  account/session under test. Record exact versions, the ProviderX commit, macOS version, transport,
  and a redacted result.
- Do not modify global Codex configuration, restart ChatGPT Desktop, or interrupt active tasks merely
  to run a probe. Prefer isolated ports, temporary homes, ephemeral tasks, and session-scoped
  configuration.
- For Desktop integration shutdown or restoration, wait for the test turn to finish, restore the
  managed configuration safely, confirm a new direct task works after a complete restart, and only
  then stop the proxy.

## Change Hygiene

- Keep changes narrowly scoped and preserve unrelated working-tree edits.
- Do not commit, push, rewrite history, or create a release unless the user explicitly asks.
- Update documentation and tests when behavior, configuration, safety rules, or user-visible
  restart requirements change.
- Before handing off, summarize what changed, list the exact checks run and their results, and call
  out anything not verified. Never claim broader validation than the evidence supports.
