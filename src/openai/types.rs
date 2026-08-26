use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::anthropic::types::{
    CacheControl, Message, MessagesRequest, OutputConfig, SystemMessage, Thinking, Tool,
};

pub const DEFAULT_OPENAI_COMPAT_MODEL: &str = "claude-sonnet-4.5";
const DEFAULT_MAX_TOKENS: i32 = 32000;

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<OpenAIMessage>,
    #[serde(default)]
    pub stream: bool,
    pub max_tokens: Option<i32>,
    pub max_completion_tokens: Option<i32>,
    #[serde(default)]
    pub tools: Vec<OpenAITool>,
    pub tool_choice: Option<Value>,
    pub reasoning_effort: Option<String>,
    pub reasoning: Option<Value>,
    pub stream_options: Option<StreamOptions>,
    /// OpenAI 会话亲和键（Codex 等客户端下发），映射为 Kiro metadata 会话 UUID
    pub prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAIMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<OpenAIToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAIToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", default = "default_function_type")]
    pub tool_type: String,
    pub function: OpenAIFunctionCall,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenAIFunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAITool {
    #[serde(rename = "type", default = "default_function_type")]
    pub tool_type: String,
    pub function: Option<OpenAIToolFunction>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub parameters: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIToolFunction {
    pub name: String,
    pub description: Option<String>,
    pub parameters: Option<Value>,
}

#[derive(Debug)]
pub struct ConvertedOpenAIRequest {
    pub anthropic: MessagesRequest,
    #[allow(dead_code)] // 2-3 重接 previous_response_id 历史时恢复使用
    pub openai_messages: Vec<OpenAIMessage>,
}

#[derive(Debug)]
pub struct OpenAIConversionError {
    pub message: String,
}

impl std::fmt::Display for OpenAIConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OpenAIConversionError {}

fn default_function_type() -> String {
    "function".to_string()
}

fn err(message: impl Into<String>) -> OpenAIConversionError {
    OpenAIConversionError {
        message: message.into(),
    }
}

pub fn openai_model_to_kiro_model(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return DEFAULT_OPENAI_COMPAT_MODEL.to_string();
    }

    let lower = trimmed.to_ascii_lowercase();
    let looks_openai_native = lower.starts_with("gpt-")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.starts_with("codex");
    if looks_openai_native {
        DEFAULT_OPENAI_COMPAT_MODEL.to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn chat_to_anthropic(
    req: &ChatCompletionRequest,
    metadata: Option<crate::anthropic::types::Metadata>,
) -> Result<ConvertedOpenAIRequest, OpenAIConversionError> {
    if req.messages.is_empty() {
        return Err(err("messages must contain at least one message"));
    }

    let model = openai_model_to_kiro_model(&req.model);
    let (system, messages) = split_chat_messages(&req.messages)?;
    if messages.is_empty() {
        return Err(err("messages must contain at least one non-system message"));
    }

    let (thinking, output_config) = openai_reasoning_to_anthropic(
        req.reasoning_effort.as_deref(),
        req.reasoning.as_ref(),
    );

    let tools = convert_openai_tools(&req.tools);
    // OpenAI/Codex 客户端带 web_search 时强制走 agentic loop：纯快速路径恒返回 SSE 且
    // 只吐原始 web_search_tool_result 块，OpenAI 层既无法解析（非流式 502）也无法合成答案。
    let force_web_search_loop = tools
        .as_ref()
        .is_some_and(|list| list.iter().any(|t| t.name == "web_search"));

    Ok(ConvertedOpenAIRequest {
        anthropic: MessagesRequest {
            model,
            max_tokens: req
                .max_completion_tokens
                .or(req.max_tokens)
                .unwrap_or(DEFAULT_MAX_TOKENS)
                .max(1),
            messages,
            stream: req.stream,
            system,
            tools,
            tool_choice: convert_tool_choice(req.tool_choice.as_ref()),
            thinking,
            output_config,
            metadata,
            force_web_search_loop,
        },
        openai_messages: req.messages.clone(),
    })
}

#[allow(dead_code)] // 2-3 重接 previous_response_id 历史时恢复使用
pub fn append_openai_messages(
    mut out: Vec<OpenAIMessage>,
    messages: Vec<OpenAIMessage>,
) -> Vec<OpenAIMessage> {
    for msg in messages {
        let merge_tool_calls = out
            .last()
            .is_some_and(|last| {
                last.role == "assistant"
                    && !last.tool_calls.is_empty()
                    && msg.role == "assistant"
                    && !msg.tool_calls.is_empty()
                    && content_to_text(last.content.as_ref().unwrap_or(&Value::Null))
                        .trim()
                        .is_empty()
                    && content_to_text(msg.content.as_ref().unwrap_or(&Value::Null))
                        .trim()
                        .is_empty()
            });
        if merge_tool_calls {
            if let Some(last) = out.last_mut() {
                last.tool_calls.extend(msg.tool_calls);
            }
        } else {
            out.push(msg);
        }
    }
    out
}

fn split_chat_messages(
    messages: &[OpenAIMessage],
) -> Result<(Option<Vec<SystemMessage>>, Vec<Message>), OpenAIConversionError> {
    let mut system = Vec::new();
    let mut out = Vec::new();
    // 缓冲连续的 OpenAI `tool` 消息：Anthropic 要求同一 assistant 轮次的所有
    // tool_result 必须**合并进一条 user 消息**（且紧跟在发起 tool_use 的 assistant
    // 之后）。OpenAI 的并行工具调用会发多条独立 tool 消息，若逐条转成单独的 user
    // 消息会产生连续 user 消息、破坏 tool_use/tool_result 配对，上游报
    // 400 "tool_use and tool_result blocks must be correctly paired and ordered"。
    let mut pending_tool_results: Vec<Value> = Vec::new();

    for msg in messages {
        // 遇到任何非 tool 消息前，先把缓冲的 tool_result 作为一条 user 消息落盘。
        if msg.role != "tool" && !pending_tool_results.is_empty() {
            out.push(Message {
                role: "user".to_string(),
                content: Value::Array(std::mem::take(&mut pending_tool_results)),
            });
        }

        match msg.role.as_str() {
            "system" | "developer" => {
                let text = content_to_text(msg.content.as_ref().unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    system.push(SystemMessage {
                        text,
                        cache_control: None,
                    });
                }
            }
            "user" => out.push(Message {
                role: "user".to_string(),
                content: openai_content_to_anthropic(msg.content.as_ref())?,
            }),
            "assistant" => out.push(Message {
                role: "assistant".to_string(),
                content: assistant_content_to_anthropic(msg)?,
            }),
            "tool" => pending_tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": msg.tool_call_id.clone().unwrap_or_default(),
                "content": tool_result_content(msg.content.as_ref())?
            })),
            other => {
                tracing::debug!("忽略不支持的 OpenAI message role: {}", other);
            }
        }
    }

    // 收尾：flush 末尾残留的 tool_result（对话以工具结果结束是常见的续跑场景）。
    if !pending_tool_results.is_empty() {
        out.push(Message {
            role: "user".to_string(),
            content: Value::Array(pending_tool_results),
        });
    }

    Ok(((!system.is_empty()).then_some(system), out))
}

fn assistant_content_to_anthropic(msg: &OpenAIMessage) -> Result<Value, OpenAIConversionError> {
    let mut blocks = content_to_text_blocks(msg.content.as_ref())?;
    for call in &msg.tool_calls {
        if call.tool_type != "function" {
            continue;
        }
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id.clone().unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple())),
            "name": call.function.name,
            "input": parse_tool_arguments(&call.function.arguments),
        }));
    }

    if blocks.is_empty() {
        Ok(Value::String(String::new()))
    } else {
        Ok(Value::Array(blocks))
    }
}

fn openai_content_to_anthropic(content: Option<&Value>) -> Result<Value, OpenAIConversionError> {
    let Some(content) = content else {
        return Ok(Value::String(String::new()));
    };
    match content {
        Value::String(_) => Ok(content.clone()),
        Value::Array(_) => {
            let blocks = content_to_text_blocks(Some(content))?;
            Ok(Value::Array(blocks))
        }
        Value::Null => Ok(Value::String(String::new())),
        other => Ok(Value::String(content_to_text(other))),
    }
}

fn content_to_text_blocks(content: Option<&Value>) -> Result<Vec<Value>, OpenAIConversionError> {
    let Some(content) = content else {
        return Ok(Vec::new());
    };

    match content {
        Value::String(text) => {
            if text.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![json!({"type": "text", "text": text})])
            }
        }
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                append_content_part(&mut blocks, part)?;
            }
            Ok(blocks)
        }
        Value::Object(_) => {
            let mut blocks = Vec::new();
            append_content_part(&mut blocks, content)?;
            Ok(blocks)
        }
        Value::Null => Ok(Vec::new()),
        other => Ok(vec![json!({"type": "text", "text": other.to_string()})]),
    }
}

fn append_content_part(blocks: &mut Vec<Value>, part: &Value) -> Result<(), OpenAIConversionError> {
    let Some(obj) = part.as_object() else {
        return Ok(());
    };
    let typ = obj.get("type").and_then(Value::as_str).unwrap_or_default();
    match typ {
        "text" | "input_text" | "output_text" => {
            if let Some(text) = obj.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                blocks.push(json!({"type": "text", "text": text}));
            }
        }
        "image_url" | "input_image" | "image" => {
            if let Some(block) = image_part_to_anthropic(part)? {
                blocks.push(block);
            }
        }
        _ => {
            if let Some(text) = obj.get("text").and_then(Value::as_str)
                && !text.is_empty()
            {
                blocks.push(json!({"type": "text", "text": text}));
            }
        }
    }
    Ok(())
}

fn image_part_to_anthropic(part: &Value) -> Result<Option<Value>, OpenAIConversionError> {
    let Some(obj) = part.as_object() else {
        return Ok(None);
    };
    let url = obj
        .get("image_url")
        .and_then(|v| {
            v.as_object()
                .and_then(|o| o.get("url").and_then(Value::as_str))
                .or_else(|| v.as_str())
        })
        .or_else(|| obj.get("url").and_then(Value::as_str));

    let Some(url) = url else {
        return Ok(None);
    };

    if let Some((media_type, data)) = parse_data_url(url) {
        return Ok(Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data
            }
        })));
    }

    Ok(Some(json!({
        "type": "text",
        "text": format!("[image_url: {}]", url)
    })))
}

fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some((media_type, data))
}

fn tool_result_content(content: Option<&Value>) -> Result<Value, OpenAIConversionError> {
    let Some(content) = content else {
        return Ok(Value::String(String::new()));
    };

    match content {
        Value::String(_) => Ok(content.clone()),
        Value::Array(_) | Value::Object(_) => {
            let blocks = content_to_text_blocks(Some(content))?;
            if blocks.is_empty() {
                Ok(Value::String(content_to_text(content)))
            } else {
                Ok(Value::Array(blocks))
            }
        }
        Value::Null => Ok(Value::String(String::new())),
        other => Ok(Value::String(other.to_string())),
    }
}

/// OpenAI 内置 web search 工具的默认 max_uses（与 Anthropic 原生路径 websearch.rs 对齐）。
const DEFAULT_WEB_SEARCH_MAX_USES: i32 = 8;

/// 判断 OpenAI 工具是否为内置 web search 工具。
///
/// OpenAI/Codex 的 web search 工具类型有多个变体：`web_search`（Responses API 新版）、
/// `web_search_preview` / `web_search_preview_2025_03_11`（早期预览）。统一识别后
/// 归一化为 Anthropic 原生 `web_search_20250305`，否则会在 `tool_type != "function"`
/// 处被直接丢弃，导致 Codex 的联网搜索能力在 OpenAI 兼容层彻底失效。
/// 识别 OpenAI web_search 工具声明（含 `web_search_preview` 等 `web_search_` 前缀变体）。
/// Chat 与 Responses 两路径共用，保证变体声明在任一端点都被注入原生 web_search。
pub(crate) fn is_openai_web_search_tool(tool_type: &str) -> bool {
    tool_type == "web_search" || tool_type.starts_with("web_search_")
}

fn convert_openai_tools(tools: &[OpenAITool]) -> Option<Vec<Tool>> {
    let mut out = Vec::new();
    for tool in tools {
        // 内置 web search：转成 Anthropic 原生工具，交给后端 web_search 路由/agentic loop。
        if is_openai_web_search_tool(&tool.tool_type) {
            let max_uses = tool
                .parameters
                .as_ref()
                .and_then(|p| p.get("max_uses").or_else(|| p.get("max_num_results")))
                .and_then(Value::as_i64)
                .map(|v| v as i32)
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_WEB_SEARCH_MAX_USES);
            out.push(Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: String::new(),
                input_schema: BTreeMap::new(),
                max_uses: Some(max_uses),
                cache_control: None::<CacheControl>,
            });
            continue;
        }
        if tool.tool_type != "function" {
            continue;
        }
        let Some(function) = tool_function(tool) else {
            continue;
        };
        let name = function.0.trim();
        if name.is_empty() {
            continue;
        }
        out.push(Tool {
            tool_type: None,
            name: name.to_string(),
            description: function.1.unwrap_or_else(|| name.to_string()),
            input_schema: schema_to_btree(function.2),
            max_uses: None,
            cache_control: None::<CacheControl>,
        });
    }
    (!out.is_empty()).then_some(out)
}

fn tool_function(tool: &OpenAITool) -> Option<(&str, Option<String>, Option<Value>)> {
    if let Some(function) = &tool.function {
        Some((
            function.name.as_str(),
            function.description.clone(),
            function.parameters.clone(),
        ))
    } else {
        Some((
            tool.name.as_deref()?,
            tool.description.clone(),
            tool.parameters.clone(),
        ))
    }
}

fn schema_to_btree(schema: Option<Value>) -> BTreeMap<String, Value> {
    let schema = schema.unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    match schema {
        Value::Object(map) => map.into_iter().collect(),
        _ => {
            let mut map = BTreeMap::new();
            map.insert("type".to_string(), Value::String("object".to_string()));
            map.insert("properties".to_string(), Value::Object(Map::new()));
            map
        }
    }
}

fn convert_tool_choice(choice: Option<&Value>) -> Option<Value> {
    let choice = choice?;
    if let Some(s) = choice.as_str() {
        return Some(match s {
            "none" => json!({"type": "none"}),
            "auto" => json!({"type": "auto"}),
            "required" => json!({"type": "any"}),
            _ => choice.clone(),
        });
    }
    let function_name = choice
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str);
    if let Some(name) = function_name {
        return Some(json!({"type": "tool", "name": name}));
    }
    Some(choice.clone())
}

/// OpenAI effort 归一化（Chat 与 Responses 两路径共用）：返回 (thinking, output_config)。
pub(crate) fn openai_reasoning_to_anthropic(
    reasoning_effort: Option<&str>,
    reasoning: Option<&Value>,
) -> (Option<Thinking>, Option<OutputConfig>) {
    let effort = reasoning_effort
        .map(str::to_string)
        .or_else(|| {
            reasoning
                .and_then(|v| v.get("effort"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());

    let Some(effort) = effort else {
        return (None, None);
    };

    // Codex/OpenAI 的 effort 档位（none/minimal/low/medium/high/xhigh）归一化为后端
    // `EffortTier` 认得的值（low/medium/high/xhigh），并给出对应 thinking 预算：
    // - "none"：显式关闭推理，不下发 thinking / output_config。若原样透传，后端
    //   `EffortTier::parse("none")` 会失败并 fallback 到 high，等于把“关推理”变成高强度推理。
    // - "minimal"：后端无此档，降级到最低的 low（原样透传同样会被 parse 拒绝 → fallback high）。
    // - 其他未知值：兜底 medium。
    let (budget, normalized_effort) = match effort.as_str() {
        "none" => return (None, None),
        "minimal" | "low" => (4_000, "low"),
        "medium" => (12_000, "medium"),
        "high" => (20_000, "high"),
        "xhigh" | "max" => (20_000, "xhigh"),
        _ => (12_000, "medium"),
    };

    (
        Some(Thinking {
            thinking_type: "enabled".to_string(),
            budget_tokens: budget,
        }),
        Some(OutputConfig {
            effort: normalized_effort.to_string(),
        }),
    )
}

fn parse_tool_arguments(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "arguments": arguments }))
}

pub fn content_to_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(_) | Value::Bool(_) => content.to_string(),
        Value::Array(items) => items.iter().map(content_to_text).collect::<Vec<_>>().join(""),
        Value::Object(obj) => {
            if let Some(text) = obj.get("text").and_then(Value::as_str) {
                return text.to_string();
            }
            if let Some(text) = obj.get("content").map(content_to_text)
                && !text.is_empty()
            {
                return text;
            }
            if let Some(output) = obj.get("output").map(content_to_text)
                && !output.is_empty()
            {
                return output;
            }
            String::new()
        }
    }
}

#[derive(Debug, Clone)]
pub struct AssistantParts {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<OpenAIToolCall>,
    pub stop_reason: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub model: String,
    /// 上游 meteringEvent 透传的 credit_* 计费元数据，未下发时为 None。
    pub credit_usage: Option<f64>,
    pub credit_unit: Option<String>,
    pub credit_unit_plural: Option<String>,
}

pub fn assistant_parts_from_anthropic(value: &Value) -> AssistantParts {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    if let Some(items) = value.get("content").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                "text" => {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
                "thinking" => {
                    if let Some(t) = item.get("thinking").and_then(Value::as_str) {
                        reasoning.push_str(t);
                    }
                }
                "tool_use" => {
                    let id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let input = item.get("input").cloned().unwrap_or_else(|| json!({}));
                    tool_calls.push(OpenAIToolCall {
                        id: Some(id),
                        tool_type: "function".to_string(),
                        function: OpenAIFunctionCall {
                            name,
                            arguments: serde_json::to_string(&input)
                                .unwrap_or_else(|_| "{}".to_string()),
                        },
                    });
                }
                _ => {}
            }
        }
    }

    let usage = value.get("usage").unwrap_or(&Value::Null);
    // usage 负数消毒：上游异常时可能下发负值，对外一律截断为 0
    let token = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_i64)
            .unwrap_or_default()
            .max(0)
    };
    AssistantParts {
        text,
        reasoning,
        tool_calls,
        stop_reason: value
            .get("stop_reason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn")
            .to_string(),
        input_tokens: token("input_tokens"),
        output_tokens: token("output_tokens"),
        cache_read_tokens: token("cache_read_input_tokens"),
        cache_creation_tokens: token("cache_creation_input_tokens"),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_OPENAI_COMPAT_MODEL)
            .to_string(),
        credit_usage: usage.get("credit_usage").and_then(Value::as_f64),
        credit_unit: usage
            .get("credit_unit")
            .and_then(Value::as_str)
            .map(str::to_string),
        credit_unit_plural: usage
            .get("credit_unit_plural")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

pub fn finish_reason_from_anthropic(stop_reason: &str, has_tool_calls: bool) -> &'static str {
    match stop_reason {
        "tool_use" => "tool_calls",
        "max_tokens" | "model_context_window_exceeded" => "length",
        _ if has_tool_calls => "tool_calls",
        _ => "stop",
    }
}

pub fn chat_message_from_parts(parts: &AssistantParts) -> Value {
    let content = if parts.text.is_empty() && !parts.tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(parts.text.clone())
    };
    let mut message = json!({
        "role": "assistant",
        "content": content,
    });
    if !parts.reasoning.is_empty() {
        message["reasoning_content"] = Value::String(parts.reasoning.clone());
    }
    if !parts.tool_calls.is_empty() {
        message["tool_calls"] = json!(parts.tool_calls);
    }
    message
}

pub fn usage_json(parts: &AssistantParts) -> Value {
    let prompt_tokens = parts.input_tokens + parts.cache_creation_tokens + parts.cache_read_tokens;
    let mut usage = json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": parts.output_tokens,
        "total_tokens": prompt_tokens + parts.output_tokens,
        "prompt_tokens_details": {
            "cached_tokens": parts.cache_read_tokens,
        },
        "completion_tokens_details": {
            "reasoning_tokens": 0,
        }
    });
    // 仅在拿到上游 meteringEvent 时透传 credit_* 计费元数据
    if let Some(credit_usage) = parts.credit_usage {
        usage["credit_usage"] = json!(credit_usage);
        if let Some(unit) = &parts.credit_unit {
            usage["credit_unit"] = json!(unit);
        }
        if let Some(unit_plural) = &parts.credit_unit_plural {
            usage["credit_unit_plural"] = json!(unit_plural);
        }
    }
    usage
}

pub fn openai_error(message: impl Into<String>, error_type: impl Into<String>) -> Value {
    // 与上游 v0.8.0 对齐：仅 message + type，不再附带恒 null 的 param / code
    json!({
        "error": {
            "message": message.into(),
            "type": error_type.into(),
        }
    })
}

#[allow(dead_code)] // 2-3 重接 previous_response_id 历史时恢复使用
pub fn assistant_message_for_history(parts: &AssistantParts) -> OpenAIMessage {
    OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(Value::String(parts.text.clone())),
        tool_calls: parts.tool_calls.clone(),
        tool_call_id: None,
        name: None,
    }
}
