//! OpenAI-compatible API surface.
//!
//! This module is intentionally a thin adapter over the existing Anthropic
//! Messages pipeline, so auth, usage accounting, tracing, Kiro retries, tool
//! name mapping, image handling, and web-search routing stay in one place.

pub(crate) mod handlers;
pub(crate) mod parse;
pub(crate) mod responses;
mod types;
