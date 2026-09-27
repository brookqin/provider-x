use provider_x_core::ProtocolId;
use serde::Serialize;

/// A family groups its API and subscription connections in a single UI entry.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub category: &'static str,
    pub connections: Vec<PresetConnection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    OpenaiOAuth,
    ClaudeCode,
    None,
}

#[derive(Clone, Debug, Serialize)]
pub struct PresetConnection {
    pub id: &'static str,
    pub label: &'static str,
    pub mode: &'static str,
    pub protocol: ProtocolId,
    pub reasoning_policy: provider_x_core::ReasoningPolicy,
    pub endpoint: &'static str,
    pub models_endpoint: Option<&'static str>,
    pub websocket_endpoint: Option<&'static str>,
    pub credentials: CredentialKind,
}

/// Defaults are copied on selection; saved instances own their connection values.
#[must_use]
#[allow(clippy::too_many_lines)] // One declarative list makes preset coverage reviewable.
pub fn presets() -> Vec<ProviderPreset> {
    use ProtocolId::{
        AnthropicMessages as Messages, OpenaiChatCompletions as Chat, OpenaiResponses as Responses,
    };
    let connection = |id, label, protocol, endpoint| PresetConnection {
        id,
        label,
        mode: "api",
        protocol,
        reasoning_policy: provider_x_core::ReasoningPolicy::Native,
        endpoint,
        models_endpoint: None,
        websocket_endpoint: None,
        credentials: CredentialKind::ApiKey,
    };
    let family = |id, name, category, connections| ProviderPreset {
        id,
        name,
        category,
        connections,
    };
    vec![
        family(
            "openai",
            "OpenAI",
            "vendor",
            vec![
                PresetConnection {
                    websocket_endpoint: Some(crate::OPENAI_WEBSOCKET_ENDPOINT),
                    ..connection("api", "api", Responses, "https://api.openai.com/v1")
                },
                PresetConnection {
                    mode: "subscription",
                    credentials: CredentialKind::OpenaiOAuth,
                    models_endpoint: Some(crate::OPENAI_OAUTH_MODELS_ENDPOINT),
                    websocket_endpoint: Some(crate::OPENAI_OAUTH_WEBSOCKET_ENDPOINT),
                    ..connection(
                        "subscription",
                        "subscription",
                        Responses,
                        "https://chatgpt.com/backend-api/codex",
                    )
                },
            ],
        ),
        family(
            "anthropic",
            "Anthropic",
            "vendor",
            vec![
                connection("api", "api", Messages, "https://api.anthropic.com"),
                PresetConnection {
                    mode: "subscription",
                    credentials: CredentialKind::ClaudeCode,
                    ..connection("subscription", "subscription", Messages, "")
                },
            ],
        ),
        family(
            "deepseek",
            "DeepSeek",
            "vendor",
            vec![PresetConnection {
                reasoning_policy: provider_x_core::ReasoningPolicy::DeepSeek,
                ..connection("api", "api", Responses, "https://api.deepseek.com/v1")
            }],
        ),
        family(
            "kimi",
            "Kimi",
            "vendor",
            vec![
                connection("global", "global", Chat, "https://api.moonshot.ai/v1"),
                connection("china", "china", Chat, "https://api.moonshot.cn/v1"),
            ],
        ),
        family(
            "qwen",
            "Qwen",
            "vendor",
            vec![
                connection(
                    "global",
                    "global",
                    Chat,
                    "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
                ),
                connection(
                    "china",
                    "china",
                    Chat,
                    "https://dashscope.aliyuncs.com/compatible-mode/v1",
                ),
            ],
        ),
        family(
            "zai",
            "Z.ai / 智谱",
            "vendor",
            vec![
                connection("global", "global", Chat, "https://api.z.ai/api/paas/v4"),
                connection(
                    "global-coding",
                    "global_coding",
                    Chat,
                    "https://api.z.ai/api/coding/paas/v4",
                ),
                connection(
                    "china",
                    "china",
                    Chat,
                    "https://open.bigmodel.cn/api/paas/v4",
                ),
                connection(
                    "china-coding",
                    "china_coding",
                    Chat,
                    "https://open.bigmodel.cn/api/coding/paas/v4",
                ),
            ],
        ),
        family(
            "minimax",
            "MiniMax",
            "vendor",
            vec![
                connection(
                    "global",
                    "global",
                    Messages,
                    "https://api.minimax.io/anthropic",
                ),
                connection(
                    "china",
                    "china",
                    Messages,
                    "https://api.minimaxi.com/anthropic",
                ),
            ],
        ),
        family(
            "xai",
            "xAI",
            "vendor",
            vec![connection("api", "api", Responses, "https://api.x.ai/v1")],
        ),
        family(
            "openrouter",
            "OpenRouter",
            "relay",
            vec![connection(
                "api",
                "api",
                Chat,
                "https://openrouter.ai/api/v1",
            )],
        ),
        family(
            "opencode",
            "OpenCode",
            "relay",
            vec![
                connection("zen", "zen", Chat, "https://opencode.ai/zen/v1"),
                connection("go", "go", Chat, "https://opencode.ai/zen/go/v1"),
            ],
        ),
        family(
            "ollama",
            "Ollama",
            "local",
            vec![PresetConnection {
                credentials: CredentialKind::None,
                ..connection("local", "local", Chat, "http://127.0.0.1:11434/v1")
            }],
        ),
        family(
            "lmstudio",
            "LM Studio",
            "local",
            vec![PresetConnection {
                credentials: CredentialKind::None,
                ..connection("local", "local", Chat, "http://127.0.0.1:1234/v1")
            }],
        ),
        family(
            "custom",
            "Custom",
            "custom",
            vec![connection("api", "custom", Responses, "")],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::presets;

    #[test]
    fn families_own_subscription_modes_and_identifiers_are_unique() {
        let all = presets();
        let ids: std::collections::BTreeSet<_> = all.iter().map(|preset| preset.id).collect();
        assert_eq!(ids.len(), all.len());
        for id in ["openai", "anthropic"] {
            let family = all.iter().find(|preset| preset.id == id).unwrap();
            assert!(
                family
                    .connections
                    .iter()
                    .any(|connection| connection.mode == "subscription")
            );
            assert!(
                family
                    .connections
                    .iter()
                    .any(|connection| connection.mode == "api")
            );
        }
        assert!(!ids.contains("openai-oauth"));
        assert!(!ids.contains("gemini"));
        for preset in all {
            let ids: std::collections::BTreeSet<_> = preset
                .connections
                .iter()
                .map(|connection| connection.id)
                .collect();
            assert_eq!(ids.len(), preset.connections.len());
        }
    }
}
