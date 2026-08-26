use std::{collections::HashMap, convert::Infallible};

use axum::{
    Json as JsonExtractor,
    body::{Body, to_bytes},
    extract::{Extension, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::{Value, json};

use crate::anthropic::{
    handlers::post_messages,
    middleware::{AppState, KeyContext},
};

use super::types::{
    ChatCompletionRequest, OpenAIConversionError, assistant_parts_from_anthropic,
    chat_message_from_parts, chat_to_anthropic, finish_reason_from_anthropic, openai_error,
    usage_json,
};

const MAX_COLLECT_BYTES: usize = 32 * 1024 * 1024;

/// POST /v1/chat/completions
pub async fn post_chat_completions(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    headers: axum::http::HeaderMap,
    JsonExtractor(mut req): JsonExtractor<ChatCompletionRequest>,
) -> Response {
    apply_model_mapping(&state, &mut req.model);
    let include_usage = req
        .stream_options
        .as_ref()
        .is_some_and(|options| options.include_usage);
    let metadata =
        super::parse::resolve_session_metadata(req.prompt_cache_key.as_deref(), &headers);
    let converted = match chat_to_anthropic(&req, metadata) {
        Ok(converted) => converted,
        Err(e) => return conversion_error(e),
    };
    let stream = converted.anthropic.stream;
    let model = converted.anthropic.model.clone();

    let anthropic_response = post_messages(
        State(state),
        Extension(key_ctx),
        JsonExtractor(converted.anthropic),
    )
    .await;

    if stream {
        convert_chat_stream_response(anthropic_response, model, include_usage).await
    } else {
        convert_chat_non_stream_response(anthropic_response).await
    }
}

/// 请求时应用模型映射：命中配置的源模型名则原地改写为目标模型名。
///
/// 在 `chat_to_anthropic` 的启发式映射（gpt-*/o1/o3/codex → 默认兼容模型）之前执行，
/// 因此显式映射优先级更高；改写后的目标名（如 claude-opus-4.8）不匹配启发式前缀，
/// 会被透传，不会被二次改写。
fn apply_model_mapping(state: &AppState, model: &mut String) {
    if let Some(mappings) = &state.model_mappings
        && let Some(target) = mappings.resolve(model)
    {
        tracing::debug!("模型映射命中: {} → {}", model, target);
        *model = target;
    }
}

fn conversion_error(e: OpenAIConversionError) -> Response {
    openai_status_error(StatusCode::BAD_REQUEST, "invalid_request_error", e.message)
}

fn openai_status_error(
    status: StatusCode,
    error_type: impl Into<String>,
    message: impl Into<String>,
) -> Response {
    (status, Json(openai_error(message, error_type))).into_response()
}

async fn convert_chat_non_stream_response(response: Response) -> Response {
    let status = response.status();
    let body = response.into_body();
    if !status.is_success() {
        return convert_error_body(status, body).await;
    }

    let value = match collect_json_body(body).await {
        Ok(value) => value,
        Err(resp) => return resp,
    };
    let parts = assistant_parts_from_anthropic(&value);
    (
        StatusCode::OK,
        Json(json!({
            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            "object": "chat.completion",
            "created": unix_ts(),
            "model": parts.model,
            "choices": [{
                "index": 0,
                "message": chat_message_from_parts(&parts),
                "logprobs": null,
                "finish_reason": finish_reason_from_anthropic(
                    &parts.stop_reason,
                    !parts.tool_calls.is_empty(),
                ),
            }],
            "usage": usage_json(&parts),
        })),
    )
        .into_response()
}

async fn collect_json_body(body: Body) -> Result<Value, Response> {
    let bytes = match to_bytes(body, MAX_COLLECT_BYTES).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return Err(openai_status_error(
                StatusCode::BAD_GATEWAY,
                "server_error",
                format!("failed to read upstream response: {}", e),
            ));
        }
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        openai_status_error(
            StatusCode::BAD_GATEWAY,
            "server_error",
            format!("failed to parse upstream response: {}", e),
        )
    })
}

async fn convert_error_body(status: StatusCode, body: Body) -> Response {
    let bytes = to_bytes(body, MAX_COLLECT_BYTES).await.unwrap_or_default();
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
    openai_status_error(status, error_type, message)
}

async fn convert_chat_stream_response(
    response: Response,
    model: String,
    include_usage: bool,
) -> Response {
    let status = response.status();
    if !status.is_success() {
        return convert_error_body(status, response.into_body()).await;
    }

    let stream = transform_anthropic_sse(
        response.into_body(),
        ChatStreamTranslator::new(model, include_usage),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

trait AnthropicSseTranslator {
    fn handle_frame(&mut self, frame: SseFrame) -> Vec<Bytes>;
    fn finish(&mut self) -> Vec<Bytes>;
}

fn transform_anthropic_sse<T>(
    body: Body,
    translator: T,
) -> impl Stream<Item = Result<Bytes, Infallible>>
where
    T: AnthropicSseTranslator + Send + 'static,
{
    let data_stream = body.into_data_stream();
    stream::unfold(
        (data_stream, SseFrameParser::default(), translator, false),
        |(mut data_stream, mut parser, mut translator, mut finished)| async move {
            if finished {
                return None;
            }

            loop {
                match data_stream.next().await {
                    Some(Ok(chunk)) => {
                        let frames = parser.push(&chunk);
                        let mut out = Vec::new();
                        for frame in frames {
                            out.extend(translator.handle_frame(frame).into_iter().map(Ok));
                        }
                        if !out.is_empty() {
                            return Some((
                                stream::iter(out),
                                (data_stream, parser, translator, finished),
                            ));
                        }
                    }
                    Some(Err(e)) => {
                        finished = true;
                        let bytes = chat_data_sse(json!({
                            "error": {
                                "message": format!("upstream stream error: {}", e),
                                "type": "server_error",
                            }
                        }));
                        return Some((
                            stream::iter(vec![Ok(bytes)]),
                            (data_stream, parser, translator, finished),
                        ));
                    }
                    None => {
                        finished = true;
                        let mut out = Vec::new();
                        for frame in parser.finish() {
                            out.extend(translator.handle_frame(frame).into_iter().map(Ok));
                        }
                        out.extend(translator.finish().into_iter().map(Ok));
                        if out.is_empty() {
                            return None;
                        }
                        return Some((
                            stream::iter(out),
                            (data_stream, parser, translator, finished),
                        ));
                    }
                }
            }
        },
    )
    .flatten()
}

#[derive(Default)]
struct SseFrameParser {
    buffer: String,
}

#[derive(Debug)]
struct SseFrame {
    event: String,
    data: String,
}

impl SseFrameParser {
    fn push(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut frames = Vec::new();
        while let Some(pos) = self.buffer.find("\n\n") {
            let raw = self.buffer[..pos].to_string();
            self.buffer = self.buffer[pos + 2..].to_string();
            if let Some(frame) = parse_sse_frame(&raw) {
                frames.push(frame);
            }
        }
        frames
    }

    fn finish(&mut self) -> Vec<SseFrame> {
        let raw = std::mem::take(&mut self.buffer);
        parse_sse_frame(&raw).into_iter().collect()
    }
}

fn parse_sse_frame(raw: &str) -> Option<SseFrame> {
    let mut event = String::new();
    let mut data_lines = Vec::new();
    for line in raw.lines().map(|line| line.trim_end_matches('\r')) {
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        }
    }
    if event.is_empty() && data_lines.is_empty() {
        return None;
    }
    Some(SseFrame {
        event,
        data: data_lines.join("\n"),
    })
}

struct ToolStreamAcc {
    id: String,
    name: String,
    args: String,
    index: usize,
}

struct ChatStreamTranslator {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    sent_role: bool,
    done: bool,
    /// 上游 message_delta 的原始 stop_reason；finish 时结合是否发过工具调用再映射
    stop_reason: Option<String>,
    saw_tool_calls: bool,
    usage: Option<Value>,
    tools: HashMap<i64, ToolStreamAcc>,
    next_tool_index: usize,
}

impl ChatStreamTranslator {
    fn new(model: String, include_usage: bool) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            model,
            created: unix_ts(),
            include_usage,
            sent_role: false,
            done: false,
            stop_reason: None,
            saw_tool_calls: false,
            usage: None,
            tools: HashMap::new(),
            next_tool_index: 0,
        }
    }

    fn ensure_role(&mut self) -> Vec<Bytes> {
        if self.sent_role {
            return Vec::new();
        }
        self.sent_role = true;
        vec![self.chunk(json!({"role": "assistant"}), None, None)]
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>, usage: Option<Value>) -> Bytes {
        chat_data_sse(json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "logprobs": null,
                "finish_reason": finish_reason,
            }],
            "usage": usage,
        }))
    }
}

impl AnthropicSseTranslator for ChatStreamTranslator {
    fn handle_frame(&mut self, frame: SseFrame) -> Vec<Bytes> {
        if self.done || frame.event == "ping" {
            return Vec::new();
        }
        let data = match serde_json::from_str::<Value>(&frame.data) {
            Ok(data) => data,
            Err(_) => return Vec::new(),
        };

        match frame.event.as_str() {
            "message_start" => self.ensure_role(),
            "content_block_start" => {
                if data
                    .pointer("/content_block/type")
                    .and_then(Value::as_str)
                    == Some("tool_use")
                {
                    let block_index = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                    self.tools.insert(
                        block_index,
                        ToolStreamAcc {
                            id: data
                                .pointer("/content_block/id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: data
                                .pointer("/content_block/name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            args: String::new(),
                            index: self.next_tool_index,
                        },
                    );
                    self.next_tool_index += 1;
                    self.saw_tool_calls = true;
                }
                Vec::new()
            }
            "content_block_delta" => {
                let mut out = self.ensure_role();
                match data.pointer("/delta/type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = data.pointer("/delta/text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(self.chunk(json!({"content": text}), None, None));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = data.pointer("/delta/thinking").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(self.chunk(json!({"reasoning_content": text}), None, None));
                        }
                    }
                    Some("input_json_delta") => {
                        let index = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                        if let Some(tool) = self.tools.get_mut(&index)
                            && let Some(delta) =
                                data.pointer("/delta/partial_json").and_then(Value::as_str)
                        {
                            tool.args.push_str(delta);
                        }
                    }
                    _ => {}
                }
                out
            }
            "content_block_stop" => {
                let index = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                let Some(tool) = self.tools.remove(&index) else {
                    return Vec::new();
                };
                let mut out = self.ensure_role();
                out.push(self.chunk(
                    json!({
                        "tool_calls": [{
                            "index": tool.index,
                            "id": tool.id,
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "arguments": tool.args,
                            }
                        }]
                    }),
                    None,
                    None,
                ));
                out
            }
            "message_delta" => {
                self.stop_reason = data
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.usage = Some(usage_from_anthropic_delta(&data));
                Vec::new()
            }
            "message_stop" => self.finish(),
            "error" => {
                self.done = true;
                vec![
                    chat_data_sse(json!({
                        "error": data.get("error").cloned().unwrap_or(data)
                    })),
                    Bytes::from_static(b"data: [DONE]\n\n"),
                ]
            }
            _ => Vec::new(),
        }
    }

    fn finish(&mut self) -> Vec<Bytes> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = self.ensure_role();
        let finish_reason = finish_reason_from_anthropic(
            self.stop_reason.as_deref().unwrap_or("end_turn"),
            self.saw_tool_calls,
        );
        let usage = self.include_usage.then(|| self.usage.clone().unwrap_or_else(|| json!(null)));
        out.push(self.chunk(json!({}), Some(finish_reason), usage));
        out.push(Bytes::from_static(b"data: [DONE]\n\n"));
        out
    }
}

fn usage_from_anthropic_delta(data: &Value) -> Value {
    let usage = data.get("usage").unwrap_or(&Value::Null);
    // usage 负数消毒：上游异常时可能下发负值，对外一律截断为 0
    let token = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_i64)
            .unwrap_or_default()
            .max(0)
    };
    let input = token("input_tokens");
    let cache_creation = token("cache_creation_input_tokens");
    let cache_read = token("cache_read_input_tokens");
    let prompt = input + cache_creation + cache_read;
    let completion = token("output_tokens");
    let mut out = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_tokens_details": {
            "cached_tokens": cache_read,
        },
        "completion_tokens_details": {
            "reasoning_tokens": 0,
        }
    });
    // 仅在拿到上游 meteringEvent 时透传 credit_* 计费元数据
    if let Some(credit_usage) = usage.get("credit_usage").and_then(Value::as_f64) {
        out["credit_usage"] = json!(credit_usage);
        if let Some(unit) = usage.get("credit_unit").and_then(Value::as_str) {
            out["credit_unit"] = json!(unit);
        }
        if let Some(unit_plural) = usage.get("credit_unit_plural").and_then(Value::as_str) {
            out["credit_unit_plural"] = json!(unit_plural);
        }
    }
    out
}

fn chat_data_sse(value: Value) -> Bytes {
    Bytes::from(format!(
        "data: {}\n\n",
        serde_json::to_string(&value).unwrap_or_default()
    ))
}

fn unix_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde_json::{Value, json};

    use super::super::types::{ChatCompletionRequest, chat_to_anthropic};
    use super::{AnthropicSseTranslator, ChatStreamTranslator, SseFrameParser};

    /// 把一段 Anthropic SSE 原文喂给 ChatStreamTranslator，收集其产出的
    /// `data: {...}` 行并解析成 JSON 序列（chat completions 流不带 `event:` 行）。
    /// `data: [DONE]` 作为 Value::Null 占位返回，便于断言收尾。
    fn run_chat_translator(anthropic_sse: &str, include_usage: bool) -> Vec<Value> {
        let mut translator = ChatStreamTranslator::new("gpt-4o".to_string(), include_usage);
        let mut parser = SseFrameParser::default();
        let mut raw_out: Vec<Bytes> = Vec::new();
        for frame in parser.push(anthropic_sse.as_bytes()) {
            raw_out.extend(translator.handle_frame(frame));
        }
        for frame in parser.finish() {
            raw_out.extend(translator.handle_frame(frame));
        }
        raw_out.extend(translator.finish());

        let joined = raw_out
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<String>();
        let mut chunks = Vec::new();
        for block in joined.split("\n\n") {
            let Some(rest) = block.trim().strip_prefix("data:") else {
                continue;
            };
            let data = rest.trim();
            if data == "[DONE]" {
                chunks.push(Value::Null);
            } else if let Ok(v) = serde_json::from_str::<Value>(data) {
                chunks.push(v);
            }
        }
        chunks
    }

    #[test]
    fn chat_request_converts_tools_and_tool_results() {
        let req: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"},
                {"role": "user", "content": "summarize"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            }]
        }))
        .unwrap();

        let converted = chat_to_anthropic(&req, None).unwrap();
        assert_eq!(converted.anthropic.model, "claude-sonnet-4.5");
        assert_eq!(converted.anthropic.messages.len(), 4);
        assert_eq!(converted.anthropic.system.unwrap()[0].text, "be brief");
        assert_eq!(converted.anthropic.tools.unwrap()[0].name, "get_weather");
    }

    /// OpenAI/Codex 的内置 web search 工具（type=web_search / web_search_preview）
    /// 必须转成 Anthropic 原生格式（name=web_search, type 以 web_search_ 开头），
    /// 否则会在 convert_openai_tools 的 `!= "function"` 分支被丢弃，联网搜索彻底失效。
    /// 后端 is_native_web_search_tool 要求 type.starts_with("web_search_")，
    /// 注意裸 "web_search" 不满足该前缀，必须归一化。
    #[test]
    fn chat_request_converts_web_search_tool_variants() {
        for typ in ["web_search", "web_search_preview", "web_search_preview_2025_03_11"] {
            let req: ChatCompletionRequest = serde_json::from_value(json!({
                "model": "gpt-5",
                "messages": [{"role": "user", "content": "查一下今天的新闻"}],
                "tools": [{"type": typ}]
            }))
            .unwrap();
            let converted = chat_to_anthropic(&req, None).unwrap();
            let tools = converted
                .anthropic
                .tools
                .unwrap_or_else(|| panic!("web search 工具被丢弃了: type={typ}"));
            assert_eq!(tools.len(), 1, "type={typ}");
            assert_eq!(tools[0].name, "web_search", "type={typ}");
            let tt = tools[0].tool_type.as_deref().unwrap_or("");
            assert!(
                tt.starts_with("web_search_"),
                "type={typ} 归一化后 tool_type={tt} 不满足后端 starts_with(web_search_)"
            );
            assert_eq!(tools[0].max_uses, Some(8), "type={typ}");
        }
    }

    /// web search 与普通 function 工具混用时（Codex 常见场景），两者都要保留：
    /// web_search 转原生、function 照常转，交给后端 agentic loop。
    #[test]
    fn chat_request_keeps_web_search_alongside_function_tools() {
        let req: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "查天气并联网核对"}],
            "tools": [
                {"type": "web_search"},
                {"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object", "properties": {}}}}
            ]
        }))
        .unwrap();
        let converted = chat_to_anthropic(&req, None).unwrap();
        let tools = converted.anthropic.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools.iter().any(|t| t.name == "web_search"
            && t.tool_type.as_deref().is_some_and(|s| s.starts_with("web_search_"))));
        assert!(tools.iter().any(|t| t.name == "get_weather" && t.tool_type.is_none()));
    }

    /// 回归：OpenAI 并行工具调用（一条 assistant 带多个 tool_calls + 多条独立 tool
    /// 消息）必须合并成 assistant[tool_use...] + 单条 user[tool_result...]，否则连续
    /// user 消息会破坏配对，上游报 400 "tool_use and tool_result blocks must be
    /// correctly paired and ordered"。
    #[test]
    fn chat_request_batches_parallel_tool_results_into_one_user_message() {
        let req: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [
                {"role": "user", "content": "查北京和上海的天气"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_a", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"}},
                    {"id": "call_b", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"上海\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_a", "content": "北京晴"},
                {"role": "tool", "tool_call_id": "call_b", "content": "上海雨"},
                {"role": "user", "content": "总结"}
            ]
        }))
        .unwrap();

        let converted = chat_to_anthropic(&req, None).unwrap();
        let msgs = &converted.anthropic.messages;
        // user / assistant(2×tool_use) / user(2×tool_result 合并) / user(总结)
        assert_eq!(msgs.len(), 4);

        // assistant 轮次带两个 tool_use
        assert_eq!(msgs[1].role, "assistant");
        let assistant_blocks = msgs[1].content.as_array().unwrap();
        let tool_uses = assistant_blocks.iter().filter(|b| b["type"] == "tool_use").count();
        assert_eq!(tool_uses, 2);

        // 两个 tool_result 必须在同一条 user 消息里（关键：不能拆成两条）
        assert_eq!(msgs[2].role, "user");
        let results = msgs[2].content.as_array().unwrap();
        assert_eq!(results.len(), 2, "两个 tool_result 必须合并进一条 user 消息");
        assert!(results.iter().all(|r| r["type"] == "tool_result"));
        assert_eq!(results[0]["tool_use_id"], "call_a");
        assert_eq!(results[1]["tool_use_id"], "call_b");

        // 不应出现连续两条 user 消息承载 tool_result
        assert_eq!(msgs[3].role, "user"); // 这是"总结"，与上一条 tool_result user 相邻是允许的
    }


    /// chat completions 流式：纯文本。验证首个 chunk 带 role=assistant，
    /// 文本以 delta.content 增量下发，末尾 chunk 带 finish_reason=stop，
    /// 且不请求 usage 时不夹带 usage 对象，最后以 [DONE] 收尾。
    #[test]
    fn chat_stream_emits_openai_text_chunks() {
        let upstream = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );

        let chunks = run_chat_translator(upstream, false);

        // 首个 chunk 声明 role，object 恒为 chat.completion.chunk
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[0]["object"], "chat.completion.chunk");

        // 文本增量拼接完整
        let text: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "Hi there");

        // 收尾：倒数第二个 chunk 带 finish_reason，最后是 [DONE]
        assert_eq!(chunks.last().unwrap(), &Value::Null);
        let finish = chunks
            .iter()
            .find_map(|c| c["choices"][0]["finish_reason"].as_str());
        assert_eq!(finish, Some("stop"));

        // 未请求 include_usage 时不夹带 usage
        assert!(chunks.iter().all(|c| c["usage"].is_null()));
    }

    /// chat completions 流式：tool_use。验证 content_block_stop 时一次性发出
    /// 完整 tool_calls delta（index/id/function.name/arguments），
    /// finish_reason=tool_calls，且 include_usage 时末尾 chunk 带 OpenAI 口径 usage。
    #[test]
    fn chat_stream_emits_tool_calls_and_usage() {
        let upstream = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":4,\"cache_read_input_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );

        let chunks = run_chat_translator(upstream, true);

        // tool_calls delta：一次性带全量 id/name/arguments
        let tool_delta = chunks
            .iter()
            .find(|c| c["choices"][0]["delta"].get("tool_calls").is_some())
            .expect("tool_calls delta present");
        let call = &tool_delta["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], json!(0));
        assert_eq!(call["id"], "toolu_1");
        assert_eq!(call["function"]["name"], "get_weather");
        assert_eq!(call["function"]["arguments"], "{\"city\":\"Paris\"}");

        // finish_reason 映射为 tool_calls
        let finish = chunks
            .iter()
            .find_map(|c| c["choices"][0]["finish_reason"].as_str());
        assert_eq!(finish, Some("tool_calls"));

        // include_usage：末尾带 OpenAI chat 口径 usage（prompt/completion/total）
        let usage = chunks
            .iter()
            .find_map(|c| (!c["usage"].is_null()).then(|| c["usage"].clone()))
            .expect("usage present when include_usage=true");
        assert_eq!(usage["prompt_tokens"], json!(12)); // 10 + 2 cache_read
        assert_eq!(usage["completion_tokens"], json!(4));
        assert_eq!(usage["total_tokens"], json!(16));
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], json!(2));
    }
}
