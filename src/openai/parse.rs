//! OpenAI 兼容层共享解析工具
//!
//! Chat Completions 与 Responses 共用：会话亲和元数据
//! [`resolve_session_metadata`]、`data:` 图解析 [`parse_data_url`]、
//! Anthropic 响应体解析 [`parse_anthropic_message`] / [`ParsedResponse`]、
//! SSE 字节级分帧 [`take_sse_frames`] / [`parse_sse_frame`]，以及文本收集 /
//! 合并 / 时间戳辅助函数。

use axum::http::HeaderMap;
use axum::{
    body::{Body, to_bytes},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::types::openai_error;
use crate::anthropic::types::Metadata;

const MAX_ERROR_BODY: usize = 32 * 1024 * 1024;

/// 从 OpenAI 请求体或会话亲和请求头中提取并规范化 Kiro 会话 UUID。
pub(crate) fn resolve_session_metadata(
    prompt_cache_key: Option<&str>,
    headers: &HeaderMap,
) -> Option<Metadata> {
    let candidates = [
        prompt_cache_key,
        headers
            .get("x-session-affinity")
            .and_then(|value| value.to_str().ok()),
        headers
            .get("x-client-request-id")
            .and_then(|value| value.to_str().ok()),
        headers
            .get("session_id")
            .and_then(|value| value.to_str().ok()),
    ];

    candidates.into_iter().flatten().find_map(|candidate| {
        let raw_uuid = candidate.strip_prefix("session_").unwrap_or(candidate);
        let uuid = Uuid::parse_str(raw_uuid).ok()?;
        Some(Metadata {
            user_id: Some(format!("session_{uuid}")),
        })
    })
}

/// 解析 `data:` URL：`data:<media_type>;base64,<data>` → `(media_type, data)`。
///
/// 畸形输入（缺 `;base64,` 分隔）返回 `None`，由调用方降级为文本引用，
/// 不静默丢弃用户输入。Chat 与 Responses 两条路径共用。
pub(crate) fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some((media_type, data))
}

/// 追加到 merged，若与上一轮 role 相同则合并 content blocks
pub(crate) fn push_merged(merged: &mut Vec<(String, Vec<Value>)>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = merged.last_mut() {
        if last.0 == role {
            last.1.extend(blocks);
            return;
        }
    }
    merged.push((role.to_string(), blocks));
}

/// 仅收集纯文本（system / tool 内容用）
pub(crate) fn collect_text_strings(content: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                out.push(s.clone());
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(t) = part.get("text").and_then(|v| v.as_str())
                    && !t.is_empty()
                {
                    out.push(t.to_string());
                }
            }
        }
        _ => {}
    }
    out
}

// ============================ 响应翻译 ============================

pub(crate) struct ParsedResponse {
    pub(crate) model: String,
    pub(crate) text: String,
    pub(crate) tool_calls: Vec<Value>, // OpenAI tool_calls
    pub(crate) finish_reason: String,
    pub(crate) prompt_tokens: i64,
    pub(crate) cached_tokens: i64,
    pub(crate) completion_tokens: i64,
    /// 思考文本（content 里的 thinking 块 + web_search loop 的顶层
    /// `kiro_thinking` 带外字段）。chat/completions 路径不消费，
    /// Responses 路径渲染为 reasoning summary item。
    pub(crate) thinking: String,
    /// 内部代答的 web_search 展示（server_tool_use 块）：(id, query)。
    /// Responses 路径渲染为 web_search_call item。
    pub(crate) web_searches: Vec<(String, String)>,
    /// 上游 meteringEvent 透传的 credit_usage，未下发时为 None。
    /// 仅在拿到 meteringEvent 时才把 credit_usage / credit_unit /
    /// credit_unit_plural 写入响应 usage。
    pub(crate) credit_usage: Option<f64>,
    pub(crate) credit_unit: Option<String>,
    pub(crate) credit_unit_plural: Option<String>,
}

pub(crate) fn parse_anthropic_message(anthropic: &Value, model: &str) -> ParsedResponse {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut thinking = String::new();
    let mut web_searches = Vec::new();

    if let Some(blocks) = anthropic.get("content").and_then(|v| v.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        text.push_str(t);
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                        thinking.push_str(t);
                    }
                }
                Some("server_tool_use") => {
                    // 内部代答的 web_search 展示块（websearch_loop Contract A）
                    if block.get("name").and_then(|v| v.as_str()) == Some("web_search") {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let query = block
                            .pointer("/input/query")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        web_searches.push((id, query));
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let arguments = block
                        .get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    }));
                }
                _ => {} // web_search_tool_result / 其它块对 OpenAI 客户端无意义，忽略
            }
        }
    }

    // web_search loop 的带外思考文本（不进 content，避免 Anthropic 客户端回放）
    if let Some(t) = anthropic.get("kiro_thinking").and_then(|v| v.as_str())
        && !t.is_empty()
    {
        if !thinking.is_empty() {
            thinking.push_str("\n\n");
        }
        thinking.push_str(t);
    }

    let stop_reason = anthropic
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");
    let finish_reason = map_finish_reason(stop_reason, !tool_calls.is_empty()).to_string();

    let usage = anthropic.get("usage");
    let uncached_input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let cache_creation_tokens = usage
        .and_then(|u| u.get("cache_creation_input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let cached_tokens = usage
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let prompt_tokens = uncached_input_tokens
        .saturating_add(cache_creation_tokens)
        .saturating_add(cached_tokens);
    let completion_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);

    let credit_usage = usage
        .and_then(|u| u.get("credit_usage"))
        .and_then(|v| v.as_f64());
    let credit_unit = usage
        .and_then(|u| u.get("credit_unit"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let credit_unit_plural = usage
        .and_then(|u| u.get("credit_unit_plural"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    ParsedResponse {
        model: model.to_string(),
        text,
        tool_calls,
        finish_reason,
        prompt_tokens,
        cached_tokens,
        completion_tokens,
        thinking,
        web_searches,
        credit_usage,
        credit_unit,
        credit_unit_plural,
    }
}

pub(crate) fn map_finish_reason(stop_reason: &str, has_tool_calls: bool) -> &'static str {
    match stop_reason {
        "tool_use" => "tool_calls",
        "max_tokens" | "model_context_window_exceeded" => "length",
        _ if has_tool_calls => "tool_calls",
        _ => "stop",
    }
}

pub(crate) fn chat_message_from_parsed(p: &ParsedResponse) -> Value {
    let content = if p.text.is_empty() && !p.tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(p.text.clone())
    };
    let mut message = json!({
        "role": "assistant",
        "content": content,
    });
    if !p.thinking.is_empty() {
        message["reasoning_content"] = Value::String(p.thinking.clone());
    }
    if !p.tool_calls.is_empty() {
        message["tool_calls"] = json!(p.tool_calls);
    }
    message
}

pub(crate) fn chat_usage_json(p: &ParsedResponse) -> Value {
    let mut usage = json!({
        "prompt_tokens": p.prompt_tokens,
        "completion_tokens": p.completion_tokens,
        "total_tokens": p.prompt_tokens + p.completion_tokens,
        "prompt_tokens_details": {
            "cached_tokens": p.cached_tokens,
        },
        "completion_tokens_details": {
            "reasoning_tokens": 0,
        }
    });
    if let Some(credit_usage) = p.credit_usage {
        usage["credit_usage"] = json!(credit_usage);
        if let Some(unit) = &p.credit_unit {
            usage["credit_unit"] = json!(unit);
        }
        if let Some(unit_plural) = &p.credit_unit_plural {
            usage["credit_unit_plural"] = json!(unit_plural);
        }
    }
    usage
}

/// 从字节缓冲切出完整 SSE 帧（`\n\n` 或 `\r\n\r\n`）。
pub(crate) fn take_sse_frames(buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    loop {
        let lf = buffer.windows(2).position(|window| window == b"\n\n");
        let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
        let delimiter = match (lf, crlf) {
            (Some(a), Some(b)) if a <= b => Some((a, 2)),
            (Some(_), Some(b)) => Some((b, 4)),
            (Some(a), None) => Some((a, 2)),
            (None, Some(b)) => Some((b, 4)),
            (None, None) => None,
        };
        let Some((position, length)) = delimiter else {
            break;
        };
        let frame = buffer.drain(..position).collect::<Vec<_>>();
        buffer.drain(..length);
        if frame.iter().any(|byte| !byte.is_ascii_whitespace()) {
            frames.push(frame);
        }
    }
    frames
}

/// 解析一帧上游 Anthropic SSE：严格 UTF-8，跳过注释，ping 不要求 JSON data。
pub(crate) fn parse_sse_frame(frame: &[u8]) -> Result<Option<(String, Value)>, String> {
    let text = std::str::from_utf8(frame)
        .map_err(|error| format!("upstream sent invalid UTF-8 SSE: {error}"))?;
    let mut event = None;
    let mut data_lines = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start());
        }
    }
    let Some(event) = event else {
        return Ok(None);
    };
    if event == "ping" {
        return Ok(Some((event, json!({ "type": "ping" }))));
    }
    let data = serde_json::from_str::<Value>(&data_lines.join("\n"))
        .map_err(|error| format!("failed to parse upstream SSE event {event}: {error}"))?;
    Ok(Some((event, data)))
}

pub(crate) fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 把内部 Anthropic 错误体归一化为 OpenAI `{"error":{message,type}}` 形状。
///
/// 非 JSON 体用 lossy 文本作 message；缺 `error.type` 时补 `server_error`。
pub(crate) async fn convert_error_body(status: StatusCode, body: Body) -> Response {
    let bytes = to_bytes(body, MAX_ERROR_BODY).await.unwrap_or_default();
    let parsed = serde_json::from_slice::<Value>(&bytes).ok();
    let message = parsed
        .as_ref()
        .and_then(|v| v.pointer("/error/message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(&bytes).to_string());
    let error_type = parsed
        .as_ref()
        .and_then(|v| v.pointer("/error/type"))
        .and_then(Value::as_str)
        .unwrap_or("server_error");
    (status, Json(openai_error(message, error_type))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_anthropic_message_extracts_credit_fields() {
        let anthropic = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "credit_usage": 1.5,
                "credit_unit": "credit",
                "credit_unit_plural": "credits"
            }
        });
        let p = parse_anthropic_message(&anthropic, "m");
        assert_eq!(p.credit_usage, Some(1.5));
        assert_eq!(p.credit_unit.as_deref(), Some("credit"));
        assert_eq!(p.credit_unit_plural.as_deref(), Some("credits"));
    }

    #[test]
    fn parse_anthropic_message_combines_all_input_categories() {
        let anthropic = json!({
            "content": [],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 3,
                "cache_creation_input_tokens": 4,
                "cache_read_input_tokens": 5,
                "output_tokens": 6
            }
        });
        let p = parse_anthropic_message(&anthropic, "m");
        assert_eq!(p.prompt_tokens, 12);
        assert_eq!(p.cached_tokens, 5);
        assert_eq!(p.completion_tokens, 6);
    }

    #[test]
    fn parse_anthropic_message_sanitizes_negative_and_missing_usage() {
        let anthropic = json!({
            "content": [{"type": "text", "text": "x"}],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": -7, "output_tokens": -3 }
        });
        let p = parse_anthropic_message(&anthropic, "m");
        assert_eq!(p.prompt_tokens, 0);
        assert_eq!(p.completion_tokens, 0);
    }

    #[test]
    fn parse_anthropic_message_without_credit_fields_leaves_them_none() {
        let anthropic = json!({
            "content": [],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
        let p = parse_anthropic_message(&anthropic, "m");
        assert!(p.credit_usage.is_none());
        assert!(p.credit_unit.is_none());
        assert!(p.credit_unit_plural.is_none());
    }

    #[tokio::test]
    async fn convert_error_body_normalizes_non_json_and_sets_content_type() {
        let resp = convert_error_body(
            StatusCode::TOO_MANY_REQUESTS,
            Body::from("not-json retry later"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with("application/json"),
            "content-type={content_type}"
        );
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("type").is_none());
        assert_eq!(v["error"]["type"], "server_error");
        assert_eq!(v["error"]["message"], "not-json retry later");
    }

    #[tokio::test]
    async fn convert_error_body_strips_anthropic_envelope() {
        let inner = json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "slow down"}
        });
        let resp =
            convert_error_body(StatusCode::TOO_MANY_REQUESTS, Body::from(inner.to_string())).await;
        let bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v.get("type").is_none());
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert_eq!(v["error"]["message"], "slow down");
    }

    #[test]
    fn chat_message_and_usage_from_parsed_response() {
        let anthropic = json!({
            "content": [
                {"type": "thinking", "thinking": "plan"},
                {"type": "text", "text": ""},
                {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "Paris"}}
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 3,
                "cache_creation_input_tokens": 4,
                "cache_read_input_tokens": 5,
                "output_tokens": 6,
                "credit_usage": 0.25
            }
        });
        let p = parse_anthropic_message(&anthropic, "gpt-4o");
        assert_eq!(p.finish_reason, "tool_calls");
        let message = chat_message_from_parsed(&p);
        assert_eq!(message["content"], Value::Null);
        assert_eq!(message["reasoning_content"], "plan");
        assert_eq!(message["tool_calls"][0]["id"], "call_1");
        let usage = chat_usage_json(&p);
        assert_eq!(usage["prompt_tokens"], 12);
        assert_eq!(usage["completion_tokens"], 6);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 5);
        assert_eq!(usage["credit_usage"], 0.25);
    }

    #[test]
    fn sse_parser_handles_chunk_boundaries_and_crlf() {
        let mut buffer = b"event: ping\r\ndata: {}\r\n\r".to_vec();
        assert!(take_sse_frames(&mut buffer).is_empty());
        buffer.extend_from_slice(b"\nevent: message_stop\n");
        let first = take_sse_frames(&mut buffer);
        assert_eq!(first.len(), 1);
        assert_eq!(parse_sse_frame(&first[0]).unwrap().unwrap().0, "ping");
        buffer.extend_from_slice(b"data: {\"type\":\"message_stop\"}\n\n");
        let second = take_sse_frames(&mut buffer);
        assert_eq!(second.len(), 1);
        assert_eq!(
            parse_sse_frame(&second[0]).unwrap().unwrap().0,
            "message_stop"
        );
    }
}
