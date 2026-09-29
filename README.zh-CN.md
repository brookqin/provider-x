# ProviderX

[English](README.md) | [简体中文](README.zh-CN.md)

ProviderX 是一款面向 Apple Silicon macOS 的轻量级菜单栏应用，适用于已经拥有 ChatGPT 订阅并使用 Codex 或 ChatGPT Desktop 的用户。它的主要目标，是在保留 OpenAI 官方模型和订阅访问方式的前提下，以尽可能接近原生模型的使用体验扩充可用的第三方模型。

ProviderX 在本机提供受保护的 Egress Router，通过带供应商命名空间的模型扩展模型目录，并根据各供应商支持的传输方式与协议转换 Codex 的 OpenAI Responses 流量。

## 为什么开发 ProviderX？

- **保护 ChatGPT 个性化设置。** 作者的 ChatGPT 个性化设置曾被重置，因此 ProviderX 只修改 Codex 集成所必需的受管配置，保留无关设置、检测外部变更，并保存可用于恢复的回执。
- **避免引入另一套庞大运行时。** ProviderX 不会额外安装或内嵌 Chromium、浏览器、Bun 或 Node.js。
- **降低资源消耗。** 对一个小型设置应用而言，内嵌浏览器加 H5 界面过于沉重，因此 ProviderX 使用 GPUI Kit 和 gpui-omarchy 实现原生设置窗口，并作为原生 macOS 菜单栏应用运行。
- **保持功能聚焦。** ProviderX 不打算成为全功能 AI Gateway；它只希望保留主力 GPT 的原生能力，同时补充少量高性价比的第三方模型。

## 功能特性

- 裸模型 ID 仍路由至 OpenAI 官方上游，且不改变模型身份。
- `provider-a/coder` 这类带命名空间的模型会路由至对应的第三方供应商，上游仅收到 `coder`。
- 支持基于 HTTP/SSE 和原生 WebSocket 的 OpenAI Responses 协议。
- 对不支持原生 WebSocket 的供应商，将 Codex WebSocket 会话桥接至 HTTP/SSE。
- 将 Responses 请求、流式事件、工具调用和有界会话历史适配至 OpenAI Chat Completions 供应商。
- 将 Responses 请求与流式事件适配至 Anthropic Messages，包括带签名的思考块和有状态工具续接。
- 由用户主动刷新供应商模型，并将专用厂商实现映射到 [models.dev](https://models.dev/) 的厂商 ID，以精确匹配结果补充缺失元数据。
- 通过原生 GPUI 设置窗口管理供应商、模型可见性与能力、Codex 集成、开机运行、Dock 图标以及英文或简体中文界面。
- 可选择显示 Dock 图标，点击图标重新打开设置；隐藏图标时设置窗口保持打开，关闭窗口后路由服务继续运行。
- 设置按功能分组，开关使用 Switch，选项使用 Select，推理级别支持多选。外观可跟随系统；语言支持自动识别系统语言，无法匹配时回退英文。透明标题栏与窗口内容融为一体。
- 供应商主页展示已配置连接的连接方式、已选模型和启用模型数，点击整行进入编辑；新增和编辑均在弹窗内完成，移除连接需二次确认。
- 上游连接支持 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 和 `NO_PROXY`。
- 将脱敏后的请求路由、上游响应与运行错误写入按天轮换的本机私有日志，并只保留 10 天。

### 计划中

- 完整支持第三方模型的 subagent 调度，包括可读的任务详情传递和后续消息通信。

## 工作原理

```mermaid
flowchart LR
    A["Codex / ChatGPT Desktop"] --> B["ProviderX 本地 Router"]
    B -->|"裸模型 ID"| C["OpenAI 官方上游"]
    B -->|"provider/model + Responses"| D["Responses 供应商"]
    B -->|"provider/model + Chat Completions"| E["协议适配器"]
    E --> F["Chat Completions 供应商"]
    B -->|"provider/model + Anthropic Messages"| E
    E --> G["Anthropic 供应商"]
```

ProviderX 将 Codex 配置为使用形如 `http://127.0.0.1:<port>/<random-capability>/v1` 的本地地址。Router 检查顶层模型 ID 并选择路由：

- 裸 ID 视为官方模型，并透明转发。
- `<provider-id>/<model-id>` 选择已启用的第三方供应商。
- 未知或已经失效的命名空间模型会直接失败，不会回退到其他供应商。

ProviderX 会将已启用的第三方模型合并到 Codex 看到的模型列表中。供应商或集成状态发生变化后，需要完整重启 ChatGPT Desktop，其模型选择器才会刷新。

## 支持的上游协议

| 上游协议 | HTTP/SSE | 原生 WebSocket | Codex WebSocket 桥接 |
| --- | --- | --- | --- |
| OpenAI Responses | 支持 | 可选 | HTTP-only 供应商支持 |
| OpenAI Chat Completions | 支持 | 不支持 | 通过协议适配器支持 |
| Anthropic Messages | 支持 | 不支持 | 通过协议适配器支持 |

协议能力仍取决于具体上游供应商。不受支持的适配器输入项或工具会按照协议契约被拒绝或明确省略，不会被静默转换成错误语义。

## 已知限制

### 第三方 Subagent 无法接收任务详情

目前可以为 subagent 显式指定 `provider-id/model-id` 这类带命名空间的第三方模型，subagent 也可以使用该模型启动。限制发生在任务详情传递环节：Codex 原生 multi-agent v2 协议会加密 `spawn_agent`、`send_message` 和 `followup_task` 使用的 `message` 字段，并将任务正文作为 `agent_message.encrypted_content` 传递。

ProviderX 收到的只有密文，无法为第三方供应商解密或转换。因此实际表现是模型指定成功，subagent 也知道有新任务，但无法读取任务详情。声明 `multi_agent_version = "v2"` 只能让模型通过原生 subagent 的选择条件，并不表示该供应商兼容 OpenAI 的 Agent 间加密通信。这一限制不影响主 Agent 选择第三方模型。

## 环境要求

- Apple Silicon Mac（`arm64`）
- Rust 1.98.1 或更高版本及 Cargo
- Xcode Command Line Tools，包括 `codesign`、`iconutil`、`lipo` 和 `plutil`
- 所配置第三方供应商的访问凭据

ProviderX v1 不支持 Intel macOS、Linux 或 Windows。

## 从源码构建

```sh
git clone https://github.com/brookqin/provider-x.git
cd provider-x
./scripts/build-macos-app.sh
open target/macos/ProviderX.app
```

构建脚本会生成 `target/macos/ProviderX.app`，默认使用 ad-hoc 签名，并自动执行 App Bundle 验证。如需使用已经明确授权的签名身份：

```sh
PROVIDER_X_CODESIGN_IDENTITY="Developer ID Application: Example" \
  ./scripts/build-macos-app.sh
```

如需将已验证的应用打包为拖拽安装 DMG，安装
[`create-dmg`](https://github.com/create-dmg/create-dmg) 后运行：

```sh
brew install create-dmg
./scripts/create-macos-dmg.sh
```

镜像包含 `ProviderX.app` 和 Applications 快捷方式，输出到
`target/macos/ProviderX-<version>-arm64.dmg`。Release workflow 使用同一脚本，并发布 DMG 及其 SHA-256 校验和。

## 配置供应商

1. 从菜单栏打开设置，在供应商页选择厂家。
2. 选择 API、订阅或地区连接方式，填写名称及所需凭据。本地服务无需密钥。
3. 获取模型列表或手动输入模型 ID，启用需要使用的模型。能力元数据可选，不需要审核或先通过测试。
4. 保存配置。高级设置可修改 API 连接的协议、HTTP、模型发现和 WebSocket 地址。
5. 在 **应用集成** 中启用 Codex / ChatGPT Desktop 集成。
6. 完整退出并重新启动 ChatGPT Desktop，选择 `provider-id/model-id` 模型。

预设包括 OpenAI、Anthropic、DeepSeek、Kimi、Qwen、Z.ai、MiniMax、xAI、OpenRouter、
OpenCode Zen/Go、Ollama、LM Studio 和自定义连接。预设只提供创建时的默认值；保存后的实例持有自己的地址与协议。
本次是破坏性升级，供应商配置使用 schema 3。不兼容的供应商配置和模型缓存会先自动备份，再以空白供应商配置启动；提示显示备份目录，点击后进入供应商设置。Codex 接入记录保持不动，不转换或继续运行旧格式。

OpenAI 的 API 与 ChatGPT 订阅位于同一厂家入口；订阅采用浏览器 OAuth 登录并直接访问对应 HTTP/WebSocket 后端。
Anthropic 的 API Key 使用 Messages API；订阅使用用户已安装并登录的本地 Claude Code 程序。
ProviderX 不安装 Claude Code，不读取其凭据，也不会回退为直接请求 Claude 订阅 HTTP 接口。
“检查本地安装”只检查程序文件是否存在，不代表登录或推理成功。

Claude Code 桥接目前每次请求启动一个隔离进程，将调用方历史作为输入上下文；不复用 CLI 会话或承诺原生提示缓存命中。
调用方工具通过本机 MCP 描述并交回调用方执行，禁用 CLI 内置工具。此路径仅通过模拟程序验证，真实订阅联调尚未完成。当前支持文本与调用方工具，图片和文档块会被明确拒绝。
可选连接测试只验证模型发现接口，不代表所有模型或推理能力已验证。

Anthropic 默认使用 `anthropic_thinking: adaptive`；需要旧式扩展思考的接口可明确配置为 `enabled`。

停用集成时，只要 ProviderX 管理的配置值没有被外部修改，它就会恢复启用集成前的 Codex 设置。请保持 ProviderX 运行至现有任务结束，然后再重启 ChatGPT Desktop。

## 本地数据

供应商配置、模型缓存、恢复回执和界面偏好存储在：

```text
~/Library/Application Support/dev.qiankun.provider-x/
```

供应商凭据保存在本机私有配置中，包括 API Key 或账号登录产生的 OAuth Token。ProviderX 使用严格的文件权限、普通文件检查、原子写入和并发修改检测。Codex 集成只更新 `~/.codex/config.toml` 中由 ProviderX 管理的设置，并保留无关配置。

脱敏后的请求路由、上游响应与运行错误以 JSON Lines 格式写入 `logs/provider-x-YYYY-MM-DD.log`。日志按照 Mac 的本地自然日轮换，仅保留当天及此前 9 天。每条记录包含应用版本、进程运行 ID 和级别；WebSocket 记录还包含会话 ID，可将请求、上游握手和错误关联起来。错误记录包含请求方法、不含查询参数的接口路径、入口认证结果、状态码和稳定错误码；WebSocket 错误还会记录直连或 HTTP 桥接模式、路由、失败阶段、方向及脱敏后的原因分类。入口认证失败时会保留完整路径；仅当首段符合 64 位 capability 的标准格式时，才会将该段替换为 `<redacted-capability>`。日志不会包含原始认证数据、原始 capability、请求或响应正文、原始上游错误文本，或原始 Codex 配置内容。

请勿公开上述目录，也不要将其中内容直接附加到 Issue 报告中。

## 安全设计

- 本地入口仅监听 `127.0.0.1`，并通过 URL 路径中的随机 256 位 capability 进行保护。
- 拒绝带浏览器 Origin 的 WebSocket Upgrade。
- 官方凭据与第三方路由隔离；每个供应商只会收到其自身配置的认证信息。
- 请求体、流、会话历史、连接数和空闲时间均有上限。
- 取消操作和优雅退出会传播到仍在执行的上游工作。
- 对可能已经到达上游的请求绝不自动重放。
- 含敏感信息的调试输出和探针证据会经过脱敏。

ProviderX 是 Egress Router，不是凭据保险库。请妥善保护 macOS 账户和本地存储。

## 开发

本项目是使用 Rust 2024 的 Cargo Workspace。Crate 边界、架构约束、安全要求和按变更范围选择验证方式的说明见 [AGENTS.md](AGENTS.md)。

UI 开发使用 `./scripts/dev-ui.sh`，它启动隔离临时数据目录中的原生 Rust 应用。
设置页使用 gpui-omarchy 组件，修改后重新编译；不再依赖 JavaScript 运行时、脚本资源或热重载。
主题偏好保存在 ProviderX 自身目录，“跟随系统”使用 macOS 外观通知，不依赖 Omarchy 主题文件。

基础检查：

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Apple Silicon macOS App Bundle 与生命周期检查：

```sh
./scripts/build-macos-app.sh
./scripts/smoke-macos-shell.sh
./scripts/smoke-offline-ui.sh
```

真实供应商及真实 Codex/ChatGPT Desktop 探针必须显式启用。测试证据不得记录 Authorization Header、Cookie、OAuth Token、账户 ID、Attestation 数据、完整请求体或未经脱敏的本地配置。

## 许可证

ProviderX 仅以 [GNU General Public License v3.0](LICENSE) 授权。
