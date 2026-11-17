//! 推理内容事件
//!
//! 处理 reasoningContentEvent 类型的事件。

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// Kiro 原生 thinking / reasoning 事件。
#[derive(Debug, Clone, Default)]
pub struct ReasoningContentEvent {
    /// 明文思考内容片段。
    pub text: Option<String>,
    /// 思考块签名，Anthropic 客户端下一轮会原样回传。
    pub signature: Option<String>,
    /// 上游返回的加密思考内容。
    pub redacted_content: Option<String>,
}

impl<'de> Deserialize<'de> for ReasoningContentEvent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Payload {
            text: Option<String>,
            reasoning_text: Option<serde_json::Value>,
            signature: Option<String>,
            redacted_content: Option<String>,
        }
        let payload = Payload::deserialize(deserializer)?;
        let nested_text = payload.reasoning_text.as_ref().and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("text").and_then(|v| v.as_str()))
                .or_else(|| value.get("Text").and_then(|v| v.as_str()))
        });
        Ok(Self {
            text: payload.text.or_else(|| nested_text.map(str::to_owned)),
            signature: payload.signature,
            redacted_content: payload.redacted_content,
        })
    }
}

impl EventPayload for ReasoningContentEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_reasoning_preserves_flat_priority_and_opaque_fields() {
        for nested in [
            serde_json::json!("nested"),
            serde_json::json!({"text":"nested"}),
            serde_json::json!({"Text":"nested"}),
        ] {
            let value = serde_json::json!({"reasoningText":nested, "signature":"sig","redactedContent":"opaque"});
            let event: ReasoningContentEvent = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(event.text.as_deref(), Some("nested"));
            assert_eq!(event.signature.as_deref(), Some("sig"));
            assert_eq!(event.redacted_content.as_deref(), Some("opaque"));
            let mut value = value;
            value["text"] = serde_json::json!("flat");
            let event: ReasoningContentEvent = serde_json::from_value(value).unwrap();
            assert_eq!(event.text.as_deref(), Some("flat"));
        }
        let event: ReasoningContentEvent =
            serde_json::from_value(serde_json::json!({"signature":"opaque-only"})).unwrap();
        assert!(event.text.is_none());
    }

    #[test]
    fn parse_text_signature_payload() {
        let v: ReasoningContentEvent =
            serde_json::from_str(r#"{"text":"abc","signature":"sig"}"#).unwrap();
        assert_eq!(v.text.as_deref(), Some("abc"));
        assert_eq!(v.signature.as_deref(), Some("sig"));
    }

    #[test]
    fn parse_redacted_payload() {
        let v: ReasoningContentEvent =
            serde_json::from_str(r#"{"redactedContent":"encrypted"}"#).unwrap();
        assert_eq!(v.redacted_content.as_deref(), Some("encrypted"));
    }
}
