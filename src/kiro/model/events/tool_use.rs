//! 工具使用事件
//!
//! 处理 toolUseEvent 类型的事件

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 工具使用事件
///
/// 包含工具调用的流式数据
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUseEvent {
    /// 工具名称
    #[serde(default)]
    pub name: String,
    /// 工具调用 ID
    pub tool_use_id: String,
    /// 字符串是增量分片，对象是最新完整快照，不能把多个对象拼接。
    #[serde(default = "empty_input", deserialize_with = "deserialize_input")]
    pub input: serde_json::Value,
    /// 是否是最后一个块
    #[serde(default)]
    pub stop: bool,
}

fn empty_input() -> serde_json::Value {
    serde_json::Value::String(String::new())
}

fn deserialize_input<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<serde_json::Value, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_string() || value.is_object() {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(
            "tool input must be a string delta or object snapshot",
        ))
    }
}

impl EventPayload for ToolUseEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl std::fmt::Display for ToolUseEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.stop {
            write!(
                f,
                "ToolUse[{}] (id={}, complete): {}",
                self.name, self.tool_use_id, self.input
            )
        } else {
            write!(
                f,
                "ToolUse[{}] (id={}, partial): {}",
                self.name, self.tool_use_id, self.input
            )
        }
    }
}
