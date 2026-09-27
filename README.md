# ProviderX

[English](README.md) | [简体中文](README.zh-CN.md)

ProviderX is a lightweight Apple Silicon macOS menu-bar application for people who already have a
ChatGPT subscription and use Codex or ChatGPT Desktop. Its primary goal is to expand the available
third-party models in a way that stays as close as possible to the native model experience, while
preserving official OpenAI models and subscription-backed access.

It exposes a protected loopback egress router, extends the model catalog with namespaced provider
models, and adapts Codex's OpenAI Responses traffic to the transport and protocol supported by each
configured provider.

## Why ProviderX?

- **Preserve ChatGPT personalization.** The project began after the author's ChatGPT personalization
  settings were once reset. ProviderX limits itself to the Codex settings it must manage, preserves
  unrelated configuration, detects external changes, and keeps recovery data for restoration.
- **Avoid another heavyweight runtime.** ProviderX does not install or bundle another Chromium
  runtime, embedded browser, Bun, or Node.js.
- **Use fewer resources.** An embedded browser plus an H5 interface is unnecessary for a small
  settings application. ProviderX uses GPUI Kit and gpui-omarchy for a native settings window and runs as a native macOS
  menu-bar application.
- **Stay focused.** ProviderX is not intended to become an all-purpose AI gateway. Its purpose is to
  preserve the native capabilities of the primary GPT experience while adding a small selection of
  cost-effective third-party models.

## Features

- Route bare model IDs to the official OpenAI upstream without changing their identity.
- Route namespaced IDs such as `provider-a/coder` to the matching third-party provider while sending
  only `coder` upstream.
- Support OpenAI Responses over HTTP/SSE and native WebSocket.
- Bridge Codex WebSocket sessions to HTTP/SSE for providers without native WebSocket support.
- Adapt Responses requests, streaming events, tool calls, and bounded session history to OpenAI Chat
  Completions providers.
- Adapt Responses requests and streams to Anthropic Messages, including signed thinking blocks and
  stateful tool continuations.
- Discover provider models explicitly, map dedicated implementations to their
  [models.dev](https://models.dev/) provider IDs, and enrich missing metadata with exact matches.
- Manage provider settings, model visibility and capabilities, Codex integration, launch at login, Dock visibility,
  and English or Simplified Chinese UI from a native GPUI settings window.
- Optionally show a Dock icon to reopen settings. Hiding the icon keeps the settings window open;
  closing the window keeps the router running.
- Use grouped settings with switches and select menus, including multiple reasoning-level choices.
  The provider page lists each connection with its connection mode, selected models, and enabled-model count; click a row to edit it, or add a connection in a dialog. Removing a connection requires confirmation.
  Appearance can follow the system; automatic language uses the system language and falls back to
  English when unsupported. The transparent title bar blends into the window content.
- Respect `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` for upstream connections.
- Record redacted request routing, upstream responses, and runtime errors in private daily local
  logs with 10-day retention.

### Planned

- Support complete third-party subagent scheduling, including readable task delivery and follow-up
  messages.

## How It Works

```mermaid
flowchart LR
    A["Codex / ChatGPT Desktop"] --> B["ProviderX loopback router"]
    B -->|"Bare model ID"| C["Official OpenAI upstream"]
    B -->|"provider/model + Responses"| D["Responses provider"]
    B -->|"provider/model + Chat Completions"| E["Protocol adapter"]
    E --> F["Chat Completions provider"]
    B -->|"provider/model + Anthropic Messages"| E
    E --> G["Anthropic provider"]
```

ProviderX configures Codex to use a local URL shaped like
`http://127.0.0.1:<port>/<random-capability>/v1`. The router inspects the top-level model ID and
selects the route:

- Bare IDs remain official models and are transparently forwarded.
- `<provider-id>/<model-id>` selects an enabled third-party provider.
- Unknown or stale namespaced models fail closed instead of falling back to another provider.

ProviderX merges enabled third-party models into the model-list response seen by Codex. A complete
restart of ChatGPT Desktop is required after provider or integration changes before its model picker
is expected to refresh.

## Supported Upstream Protocols

| Upstream protocol | HTTP/SSE | Native WebSocket | Codex WebSocket bridge |
| --- | --- | --- | --- |
| OpenAI Responses | Yes | Optional | Yes, when the provider is HTTP-only |
| OpenAI Chat Completions | Yes | No | Yes, through the protocol adapter |
| Anthropic Messages | Yes | No | Yes, through the protocol adapter |

Protocol feature parity still depends on the upstream provider. Unsupported adapter inputs or tools
are rejected or deliberately omitted according to the protocol contract rather than being silently
misrepresented.

## Known Limitations

### Task delivery to third-party subagents

A namespaced third-party model such as `provider-id/model-id` can be explicitly selected for a
subagent, and the subagent can start with that model. The current limitation is task delivery:
Codex's native multi-agent v2 protocol encrypts the `message` fields used by `spawn_agent`,
`send_message`, and `followup_task`, then delivers the task body as
`agent_message.encrypted_content`.

ProviderX receives only that encrypted content and cannot decrypt or convert it for a third-party
provider. As a result, model selection can succeed while the subagent sees that a task exists but
cannot read its details. Declaring `multi_agent_version = "v2"` makes the model eligible for native
subagent selection; it does not make the provider compatible with OpenAI's encrypted inter-agent
communication. This limitation does not affect selecting a third-party model for the main agent.

## Requirements

- Apple Silicon Mac (`arm64`)
- Rust 1.97.1 or newer with Cargo
- Xcode Command Line Tools, including `codesign`, `iconutil`, `lipo`, and `plutil`
- Credentials for each third-party provider you choose to configure

ProviderX v1 does not run on Intel macOS, Linux, or Windows.

## Build from Source

```sh
git clone https://github.com/brookqin/provider-x.git
cd provider-x
./scripts/build-macos-app.sh
open target/macos/ProviderX.app
```

The build script creates `target/macos/ProviderX.app`, applies an ad-hoc signature by default, and
runs the bundle verifier automatically. To use an explicitly authorized signing identity:

```sh
PROVIDER_X_CODESIGN_IDENTITY="Developer ID Application: Example" \
  ./scripts/build-macos-app.sh
```

To package the verified app into a drag-to-install DMG, install
[`create-dmg`](https://github.com/create-dmg/create-dmg) and run:

```sh
brew install create-dmg
./scripts/create-macos-dmg.sh
```

The image contains `ProviderX.app` and an Applications shortcut, and is written to
`target/macos/ProviderX-<version>-arm64.dmg`. The release workflow uses the same script and
publishes the DMG with its SHA-256 checksum.

## Configure a Provider

1. Open settings from the menu bar and choose a provider family.
2. Select an API, subscription, or regional connection and supply its required credentials. Local services need no key.
3. Discover models or enter model IDs manually, then enable the models you need. Capability metadata and connection testing are optional.
4. Save. Advanced settings expose API protocol, HTTP, discovery, and WebSocket endpoints.
5. Enable Codex / ChatGPT Desktop integration under **App integration**.
6. Fully restart ChatGPT Desktop and choose a `provider-id/model-id` model.

Presets cover OpenAI, Anthropic, DeepSeek, Kimi, Qwen, Z.ai, MiniMax, xAI, OpenRouter,
OpenCode Zen/Go, Ollama, LM Studio, and custom connections. Defaults are copied when selected;
saved instances own their endpoints and protocols. This breaking upgrade requires provider
schema 3. Incompatible provider settings and model caches are automatically backed up before
starting with empty provider settings. A notice shows the backup folder and opens provider setup;
Codex integration records are preserved. Old formats are not converted or supported at runtime.

OpenAI API and ChatGPT subscription connections share one family. Subscription sign-in uses
browser OAuth with direct HTTP/WebSocket access. Anthropic API keys use Messages;
Anthropic subscriptions run the locally installed, signed-in Claude Code executable.
ProviderX does not install Claude Code, read its credentials, or fall back to direct subscription HTTP calls.
The installation check only checks for a program file; it does not verify authentication or inference.

The Claude bridge starts an isolated process for each request and supplies caller-owned history
as input context. It does not reuse CLI sessions or promise native prompt-cache continuity.
Caller tools are exposed through loopback MCP and returned to the caller for execution; built-in
CLI tools are disabled. This path has fake-process coverage only; live subscription validation remains pending. Text and caller tools are supported; image/document blocks are rejected.
Optional connection testing checks model discovery, not inference or every model capability.

Anthropic defaults to `anthropic_thinking: adaptive`; endpoints requiring manual extended
thinking can explicitly select `enabled` in the provider document.

When integration is disabled, ProviderX restores the Codex settings it previously managed as long
as those values have not been changed externally. Keep ProviderX running until active tasks finish,
then restart ChatGPT Desktop.

## Local Data

Provider configuration, model caches, recovery receipts, and UI preferences are stored under:

```text
~/Library/Application Support/dev.qiankun.provider-x/
```

Provider credentials, including API keys and OAuth tokens created by account sign-in, are stored
locally in the private provider configuration. ProviderX uses
restrictive permissions, regular-file checks, atomic writes, and concurrent-change detection. Codex
integration updates only its managed settings in `~/.codex/config.toml` and preserves unrelated
configuration.

Redacted request routing, upstream responses, and runtime errors are written as JSON Lines to
`logs/provider-x-YYYY-MM-DD.log`. Files rotate on the Mac's local calendar date, and ProviderX keeps
the current day plus the previous nine days. Every record contains the app version, process run ID,
and level. WebSocket records also contain a session ID that correlates requests, upstream
handshakes, and failures. Error records contain diagnostic fields such as the request method, path
without its query string, ingress-authorization result, status, and stable error code. WebSocket
errors also identify the direct or HTTP-bridge mode, route, failure stage, direction, and sanitized
reason category. Unauthorized paths are retained in full unless the first segment has the canonical
64-character capability shape; only that segment is replaced with `<redacted-capability>`. Logs do
not contain raw authorization data, ingress capabilities, request or response bodies, raw upstream
error text, or original Codex configuration contents.

Do not publish either directory or include its contents in issue reports.

## Security Design

- The ingress listener is restricted to `127.0.0.1` and protected by a random 256-bit capability in
  the URL path.
- Browser-origin WebSocket upgrades are rejected.
- Official credentials are isolated from third-party routes; each provider receives only its own
  configured authorization.
- Request bodies, streams, session history, connection counts, and idle periods are bounded.
- Cancellation and graceful shutdown propagate to active upstream work.
- Requests that may already have reached an upstream are never automatically replayed.
- Secret-bearing debug output and probe evidence are redacted.

ProviderX is an egress router, not a credential vault. Protect your macOS account and local storage
accordingly.

## Development

The project is a Cargo workspace using Rust 2024. See [AGENTS.md](AGENTS.md) for crate boundaries,
architectural invariants, security requirements, and change-specific validation guidance.

Run `./scripts/dev-ui.sh` for the native Rust UI with isolated temporary data.
The settings view uses gpui-omarchy components; rebuild the application after UI changes.
No JavaScript runtime, script bundle, or hot reload is required. Theme preferences are saved
in ProviderX storage. Following system appearance uses macOS notifications, not Omarchy theme files.

Baseline checks:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Apple Silicon macOS bundle and lifecycle checks:

```sh
./scripts/build-macos-app.sh
./scripts/smoke-macos-shell.sh
./scripts/smoke-offline-ui.sh
```

Live provider and real Codex/ChatGPT Desktop probes are opt-in. Never record authorization headers,
cookies, OAuth tokens, account IDs, attestation data, complete request bodies, or unredacted local
configuration as test evidence.

## License

ProviderX is licensed under the [GNU General Public License v3.0 only](LICENSE).
