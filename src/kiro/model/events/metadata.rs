//! 上游元数据事件
//!
//! Kiro 在 `metadataEvent.tokenUsage` 中返回本次模型调用的精确 token 用量。
//! 四个字段是单次调用的最终快照，不是增量事件；调用方应在同一条流内保留最后一份快照。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 单次 Kiro 模型调用的精确 token 用量。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// 未命中缓存、也未写入缓存的输入 token。
    #[serde(default)]
    pub uncached_input_tokens: i32,
    /// 模型输出 token。
    #[serde(default)]
    pub output_tokens: i32,
    /// 从服务端 prompt cache 读取的输入 token。
    #[serde(default)]
    pub cache_read_input_tokens: i32,
    /// 本次写入服务端 prompt cache 的输入 token。
    #[serde(default)]
    pub cache_write_input_tokens: i32,
}

impl TokenUsage {
    /// 清理不可信上游值，确保所有计数非负。
    pub fn sanitized(self) -> Self {
        Self {
            uncached_input_tokens: self.uncached_input_tokens.max(0),
            output_tokens: self.output_tokens.max(0),
            cache_read_input_tokens: self.cache_read_input_tokens.max(0),
            cache_write_input_tokens: self.cache_write_input_tokens.max(0),
        }
    }

    #[cfg(test)]
    /// OpenAI 口径的总输入 token（缓存读取是其中的子集）。
    pub fn total_input_tokens(self) -> i32 {
        let usage = self.sanitized();
        usage
            .uncached_input_tokens
            .saturating_add(usage.cache_write_input_tokens)
            .saturating_add(usage.cache_read_input_tokens)
    }

    /// 合并多次真实 provider 调用的用量。
    pub fn saturating_add(self, other: Self) -> Self {
        let left = self.sanitized();
        let right = other.sanitized();
        Self {
            uncached_input_tokens: left
                .uncached_input_tokens
                .saturating_add(right.uncached_input_tokens),
            output_tokens: left.output_tokens.saturating_add(right.output_tokens),
            cache_read_input_tokens: left
                .cache_read_input_tokens
                .saturating_add(right.cache_read_input_tokens),
            cache_write_input_tokens: left
                .cache_write_input_tokens
                .saturating_add(right.cache_write_input_tokens),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Refusal {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StopDetails {
    #[serde(default)]
    pub refusal: Option<Refusal>,
}

/// `metadataEvent` payload。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    /// 有些 metadataEvent 只携带 stopReason，因此 tokenUsage 必须保持可选。
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_details: Option<StopDetails>,
}

impl MetadataEvent {
    pub fn content_filter_refusal(&self) -> Option<String> {
        if !self
            .stop_reason
            .as_deref()?
            .eq_ignore_ascii_case("CONTENT_FILTERED")
        {
            return None;
        }
        let refusal = self
            .stop_details
            .as_ref()
            .and_then(|details| details.refusal.as_ref());
        Some(
            match (
                refusal.and_then(|r| r.category.as_deref()),
                refusal.and_then(|r| r.explanation.as_deref()),
            ) {
                (Some(category), Some(explanation)) => format!("{category}: {explanation}"),
                (Some(category), None) => category.to_string(),
                (None, Some(explanation)) => explanation.to_string(),
                (None, None) => "CONTENT_FILTERED".to_string(),
            },
        )
    }
}

/// 原生结束原因优先于本地推断；拒答一经观察到就不能被后续事件覆盖。
#[derive(Debug, Default)]
pub struct StopReason {
    value: Option<String>,
    native: bool,
}

impl StopReason {
    pub fn observe_metadata(&mut self, metadata: &MetadataEvent) {
        if self.get() == Some("refusal") {
            return;
        }
        let reason = match metadata
            .stop_reason
            .as_deref()
            .map(str::to_ascii_uppercase)
            .as_deref()
        {
            Some("CONTENT_FILTERED" | "REFUSAL") => "refusal",
            Some("MAX_TOKENS") => "max_tokens",
            Some("END_TURN") => "end_turn",
            Some("STOP_SEQUENCE") => "stop_sequence",
            Some("TOOL_USE") => "tool_use",
            _ => return,
        };
        if metadata.content_filter_refusal().is_some() {
            // 不把上游说明（可能包含用户内容）写入日志。
            tracing::warn!("上游内容过滤拒答");
        }
        self.value = Some(reason.to_string());
        self.native = true;
    }

    pub fn set_fallback(&mut self, reason: impl Into<String>) {
        if !self.native && self.get() != Some("refusal") {
            self.value = Some(reason.into());
        }
    }

    pub fn get(&self) -> Option<&str> {
        self.value.as_deref()
    }

    pub fn resolve(&self, has_tool_use: bool) -> &str {
        self.get()
            .unwrap_or(if has_tool_use { "tool_use" } else { "end_turn" })
    }
}

impl EventPayload for MetadataEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_filter_details_and_stop_priority() {
        let metadata: MetadataEvent = serde_json::from_str(r#"{"stopReason":"CONTENT_FILTERED","stopDetails":{"refusal":{"category":"CYBER","explanation":"cannot continue"}}}"#).unwrap();
        assert_eq!(
            metadata.content_filter_refusal().as_deref(),
            Some("CYBER: cannot continue")
        );
        let mut reason = StopReason::default();
        reason.set_fallback("model_context_window_exceeded");
        reason.observe_metadata(&MetadataEvent {
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        });
        reason.set_fallback("max_tokens");
        assert_eq!(reason.resolve(false), "end_turn");
        reason.observe_metadata(&metadata);
        reason.observe_metadata(&MetadataEvent {
            stop_reason: Some("MAX_TOKENS".into()),
            ..Default::default()
        });
        reason.set_fallback("model_context_window_exceeded");
        assert_eq!(reason.resolve(true), "refusal");
    }

    #[test]
    fn parses_official_token_usage_shape() {
        let event: MetadataEvent = serde_json::from_str(
            r#"{
                "tokenUsage": {
                    "uncachedInputTokens": 101,
                    "outputTokens": 23,
                    "cacheReadInputTokens": 300,
                    "cacheWriteInputTokens": 40
                },
                "stopReason": "end_turn"
            }"#,
        )
        .unwrap();

        let usage = event.token_usage.unwrap();
        assert_eq!(usage.uncached_input_tokens, 101);
        assert_eq!(usage.output_tokens, 23);
        assert_eq!(usage.cache_read_input_tokens, 300);
        assert_eq!(usage.cache_write_input_tokens, 40);
        assert_eq!(usage.total_input_tokens(), 441);
    }

    #[test]
    fn metadata_without_token_usage_is_not_treated_as_zero_truth() {
        let event: MetadataEvent = serde_json::from_str(r#"{"stopReason":"end_turn"}"#).unwrap();
        assert!(event.token_usage.is_none());
    }

    #[test]
    fn token_usage_with_missing_fields_defaults_only_missing_fields_to_zero() {
        let event: MetadataEvent =
            serde_json::from_str(r#"{"tokenUsage":{"outputTokens":9}}"#).unwrap();

        assert_eq!(
            event.token_usage,
            Some(TokenUsage {
                uncached_input_tokens: 0,
                output_tokens: 9,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
            })
        );
    }

    #[test]
    fn sanitizes_negative_values() {
        let usage = TokenUsage {
            uncached_input_tokens: -1,
            output_tokens: -2,
            cache_read_input_tokens: -3,
            cache_write_input_tokens: -4,
        }
        .sanitized();

        assert_eq!(usage, TokenUsage::default());
    }

    #[test]
    fn adds_multiple_provider_calls_without_overflowing() {
        let first = TokenUsage {
            uncached_input_tokens: i32::MAX,
            output_tokens: 3,
            cache_read_input_tokens: 20,
            cache_write_input_tokens: 4,
        };
        let second = TokenUsage {
            uncached_input_tokens: 7,
            output_tokens: 5,
            cache_read_input_tokens: 11,
            cache_write_input_tokens: 2,
        };

        assert_eq!(
            first.saturating_add(second),
            TokenUsage {
                uncached_input_tokens: i32::MAX,
                output_tokens: 8,
                cache_read_input_tokens: 31,
                cache_write_input_tokens: 6,
            }
        );
    }
}
