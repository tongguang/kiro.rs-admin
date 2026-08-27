//! OpenAI-compatible API surface.
//!
//! This module is intentionally a thin adapter over the existing Anthropic
//! Messages pipeline, so auth, usage accounting, tracing, Kiro retries, tool
//! name mapping, image handling, and web-search routing stay in one place.

use crate::anthropic::middleware::AppState;

pub(crate) mod handlers;
pub(crate) mod parse;
pub(crate) mod responses;
mod types;

/// 请求时应用模型映射：命中配置的源模型名则原地改写为目标模型名。
///
/// 映射是唯一的别名层：改写后（或未命中时）模型名原样透传给 converter，
/// 不再有任何启发式二次改写（gpt-*/o1/o3/codex 启发式已删除，与上游对齐）。
/// 默认 seed 含 gpt-5.5/gpt-5.4 → claude-opus-4.8，Codex 开箱即用。
///
/// fork 的用户模型映射机制：上游无此机制，随上游文件替换需补回。
/// Chat（[`handlers::post_chat_completions`]）与 Responses
///（[`responses::post_responses`]）两条路径共用。
pub(crate) fn apply_model_mapping(state: &AppState, model: &mut String) {
    if let Some(mappings) = &state.model_mappings
        && let Some(target) = mappings.resolve(model)
    {
        tracing::debug!("模型映射命中: {} → {}", model, target);
        *model = target;
    }
}
