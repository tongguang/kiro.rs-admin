use super::super::test_support::{event_stream, loopback_provider, provider_for_url};
use super::*;
use crate::model::config::{Config, ToolCompatibilityMode};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

async fn invoke(entry: usize, state: AppState, streaming: bool, disabled: bool) -> Response {
    let mixed_search = entry >= 4;
    let entry = entry % 4;
    let key = Extension(KeyContext {
        key_id: 0,
        group: None,
        key_source: crate::admin::trace_db::TraceKeySource::ClientKey,
    });
    let schema = json!({"type":"object","properties":{"x":{"type":"integer"}}});
    let model = "claude-sonnet-4.5-thinking";
    if entry < 2 {
        let mut request = json!({"model":model,"max_tokens":64,"stream":streaming,
            "messages":[{"role":"user","content":"hi"}],
            "tools":[{"name":"custom_tool","description":"test","input_schema":schema}]});
        if disabled {
            request["thinking"] = json!({"type":"disabled"});
        }
        if mixed_search {
            request["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"web_search_20250305","name":"web_search"}));
        }
        let request = JsonExtractor(serde_json::from_value(request).unwrap());
        if entry == 0 {
            post_messages(State(state), key, request).await
        } else {
            post_messages_cc(State(state), key, request).await
        }
    } else if entry == 2 {
        let mut request = json!({"model":model,"stream":streaming,
            "messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{"name":"custom_tool","parameters":schema}}]});
        if disabled {
            request["reasoning_effort"] = json!("none");
        }
        if mixed_search {
            request["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"web_search"}));
        }
        crate::openai::handlers::post_chat_completions(
            State(state),
            key,
            Default::default(),
            JsonExtractor(serde_json::from_value(request).unwrap()),
        )
        .await
    } else {
        let mut request = json!({"model":model,"stream":streaming,"store":false,"input":"hi",
            "tools":[{"type":"function","name":"custom_tool","parameters":schema}]});
        if disabled {
            request["reasoning"] = json!({"effort":"none"});
        }
        if mixed_search {
            request["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"web_search"}));
        }
        crate::openai::responses::post_responses(
            State(state),
            key,
            Default::default(),
            JsonExtractor(serde_json::from_value(request).unwrap()),
        )
        .await
    }
}

async fn serve(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (url, server)
}

#[tokio::test]
async fn event_compatibility_failures_are_not_success_on_any_entry_point() {
    use super::super::test_support::event_frame;
    let failures = [
        event_frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "ThrottlingException"),
            ],
            &json!({"message":"slow down"}),
        ),
        event_frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "AccessDeniedException"),
            ],
            &json!({"message":"denied"}),
        ),
        event_frame(
            &[
                (":message-type", "error"),
                (":error-code", "overloaded_error"),
            ],
            &json!({"message":"overloaded"}),
        ),
        event_stream(&[("metadataEvent", json!({"stopReason":"MALFORMED_TOOL_USE"}))]),
        event_stream(&[(
            "messageStopEvent",
            json!({"messageStopEvent":{"stopReason":"MALFORMED_MODEL_OUTPUT"}}),
        )]),
        event_stream(&[("metadataEvent", json!({"stopReason":"END_TURN"}))])[..9].to_vec(),
        event_stream(&[("metadataEvent", json!({"stopReason":"END_TURN"}))])[..12].to_vec(),
        {
            let mut frame = event_stream(&[("metadataEvent", json!({"stopReason":"END_TURN"}))]);
            frame.pop();
            frame
        },
    ];
    for (case, failure) in failures.iter().enumerate() {
        for entry in 0..8 {
            for streaming in [false, true] {
                let mut body = event_stream(&[
                    ("assistantResponseEvent", json!({"content":"partial"})),
                    (
                        "metadataEvent",
                        json!({"tokenUsage":{"uncachedInputTokens":3,"outputTokens":17}}),
                    ),
                ]);
                body.extend(failure);
                let (provider, server) = loopback_provider(body).await;
                let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
                let state = AppState::new(false, ToolCompatibilityMode::ClaudeCode)
                    .with_kiro_provider(provider)
                    .with_usage(None, None, Some(usage.clone()));
                let response = invoke(entry, state, streaming, false).await;
                if !streaming {
                    assert!(
                        response.status().is_client_error() || response.status().is_server_error(),
                        "case={case}, entry={entry}, status={}",
                        response.status()
                    );
                }
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                server.await.unwrap();
                let output = String::from_utf8(bytes.to_vec()).unwrap();
                assert!(
                    output.contains("\"error\""),
                    "case={case}, entry={entry}: {output}"
                );
                assert!(!output.contains("event: message_stop"), "{output}");
                assert!(!output.contains("\"finish_reason\":\"stop\""), "{output}");
                assert!(!output.contains("\"status\":\"completed\""), "{output}");
                let overview = usage.overview();
                assert_eq!(
                    (
                        overview.today_calls,
                        overview.today_errors,
                        overview.today_output_tokens
                    ),
                    (1, 1, 17),
                    "case={case}, entry={entry}, stream={streaming}: {output}"
                );
            }
        }
    }
}

#[tokio::test]
async fn event_compatibility_object_snapshots_keep_latest_parallel_arguments() {
    for stop in [false, true] {
        for entry in 0..8 {
            for streaming in [false, true] {
                let body = event_stream(&[
                    (
                        "toolUseEvent",
                        json!({"toolUseId":"call1","name":"custom_tool","input":"","stop":false}),
                    ),
                    (
                        "toolUseEvent",
                        json!({"toolUseId":"call1","name":"custom_tool","input":{"x":1}}),
                    ),
                    (
                        "toolUseEvent",
                        json!({"toolUseId":"call2","name":"custom_tool","input":{"x":3},"stop":stop}),
                    ),
                    (
                        "toolUseEvent",
                        json!({"toolUseId":"call1","input":{"x":2},"stop":stop}),
                    ),
                    (
                        "metadataEvent",
                        json!({"stopReason":"TOOL_USE","tokenUsage":{"uncachedInputTokens":3,"outputTokens":11}}),
                    ),
                ]);
                let (provider, server) = loopback_provider(body).await;
                let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
                let state = AppState::new(false, ToolCompatibilityMode::ClaudeCode)
                    .with_kiro_provider(provider)
                    .with_usage(None, None, Some(usage.clone()));
                let response = invoke(entry, state, streaming, false).await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                server.await.unwrap();
                let output = String::from_utf8(bytes.to_vec())
                    .unwrap()
                    .replace("\\\"", "\"");
                assert!(
                    output.contains("\"x\":2") && output.contains("\"x\":3"),
                    "entry={entry}, stream={streaming}, stop={stop}: {output}"
                );
                assert!(!output.contains("\"x\":1"), "{output}");
                assert!(!output.contains("\"error\""), "{output}");
                assert_eq!(
                    (
                        usage.overview().today_calls,
                        usage.overview().today_errors,
                        usage.overview().today_output_tokens
                    ),
                    (1, 0, 11)
                );
            }
        }
    }
}

#[tokio::test]
async fn event_compatibility_nested_reasoning_and_native_stops() {
    for (event, payload, anthropic, openai) in [
        (
            "messageStopEvent",
            json!({"stopReason":"MAX_TOKENS"}),
            "max_tokens",
            "length",
        ),
        (
            "metadataEvent",
            json!({"metadataEvent":{"stopReason":"MODEL_CONTEXT_WINDOW_EXCEEDED"}}),
            "model_context_window_exceeded",
            "length",
        ),
        (
            "metadataEvent",
            json!({"stopReason":"GUARDRAIL_INTERVENED"}),
            "refusal",
            "content_filter",
        ),
    ] {
        for entry in 0..8 {
            for streaming in [false, true] {
                let body = event_stream(&[
                    (
                        "reasoningContentEvent",
                        json!({"reasoningText":{"Text":"NESTED_REASONING"},"signature":"test-signature"}),
                    ),
                    ("assistantResponseEvent", json!({"content":"answer"})),
                    (event, payload.clone()),
                    (
                        "metadataEvent",
                        json!({"tokenUsage":{"uncachedInputTokens":3,"outputTokens":11}}),
                    ),
                ]);
                let (provider, server) = loopback_provider(body).await;
                let state = AppState::new(true, ToolCompatibilityMode::ClaudeCode)
                    .with_kiro_provider(provider);
                let response = invoke(entry, state, streaming, false).await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                server.await.unwrap();
                let output = String::from_utf8(bytes.to_vec()).unwrap();
                // Chat 的搜索 SSE 既有适配器不转发带外 kiro_thinking；不改其默认策略。
                assert_eq!(
                    output.contains("NESTED_REASONING"),
                    entry != 6 || !streaming,
                    "entry={entry}, stream={streaming}: {output}"
                );
                let reason = if entry % 4 < 2 {
                    anthropic
                } else if entry % 4 == 2 {
                    openai
                } else if anthropic == "refusal" {
                    "content_filter"
                } else {
                    "max_output_tokens"
                };
                assert!(
                    output.contains(reason),
                    "entry={entry}, stream={streaming}: {output}"
                );
            }
        }
    }
}

#[tokio::test]
async fn event_compatibility_normal_eof_and_length_exception_stay_valid() {
    use super::super::test_support::event_frame;
    for (ending, expected) in [
        (Vec::new(), "end_turn"),
        (
            event_stream(&[("contextUsageEvent", json!({"contextUsagePercentage":1.0}))]),
            "end_turn",
        ),
        (
            event_frame(
                &[
                    (":message-type", "exception"),
                    (":exception-type", "ContentLengthExceededException"),
                ],
                &json!({}),
            ),
            "max_tokens",
        ),
    ] {
        for entry in 0..8 {
            for streaming in [false, true] {
                let mut body = event_stream(&[
                    ("assistantResponseEvent", json!({"content":"normal answer"})),
                    (
                        "metadataEvent",
                        json!({"tokenUsage":{"uncachedInputTokens":3,"outputTokens":11}}),
                    ),
                ]);
                body.extend(&ending);
                let (provider, server) = loopback_provider(body).await;
                let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
                let state = AppState::new(false, ToolCompatibilityMode::ClaudeCode)
                    .with_kiro_provider(provider)
                    .with_usage(None, None, Some(usage.clone()));
                let response = invoke(entry, state, streaming, false).await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                server.await.unwrap();
                let output = String::from_utf8(bytes.to_vec()).unwrap();
                if streaming {
                    let text: String = output
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                        .filter_map(|value| {
                            let text = match entry % 4 {
                                0 | 1 => value["delta"]["text"].as_str(),
                                2 => value["choices"][0]["delta"]["content"].as_str(),
                                _ if value["type"] == "response.output_text.delta" => {
                                    value["delta"].as_str()
                                }
                                _ => None,
                            };
                            text.map(str::to_owned)
                        })
                        .collect();
                    assert_eq!(text, "normal answer", "{output}");
                } else {
                    assert!(output.contains("normal answer"), "{output}");
                }
                assert!(!output.contains("\"error\""), "{output}");
                let reason = if entry % 4 < 2 {
                    expected
                } else if expected == "end_turn" && entry % 4 == 2 {
                    "stop"
                } else if expected == "end_turn" {
                    "completed"
                } else if entry % 4 == 2 {
                    "length"
                } else {
                    "max_output_tokens"
                };
                assert!(
                    output.contains(reason),
                    "entry={entry}, stream={streaming}: {output}"
                );
                assert_eq!(
                    (
                        usage.overview().today_calls,
                        usage.overview().today_errors,
                        usage.overview().today_output_tokens
                    ),
                    (1, 0, 11)
                );
            }
        }
    }
}

#[tokio::test]
async fn event_compatibility_flat_metadata_priority_and_sticky_refusal() {
    for entry in 0..8 {
        for streaming in [false, true] {
            let body = event_stream(&[
                ("assistantResponseEvent", json!({"content":"answer"})),
                (
                    "metadataEvent",
                    json!({
                        "stopReason":"GUARDRAIL_INTERVENED",
                        "tokenUsage":{"uncachedInputTokens":3,"outputTokens":17,"cacheReadInputTokens":7,"cacheWriteInputTokens":4},
                        "metadataEvent":{"stopReason":"MAX_TOKENS","tokenUsage":{"uncachedInputTokens":999,"outputTokens":999}}
                    }),
                ),
                (
                    "metadataEvent",
                    json!({"metadataEvent":null,"stopReason":"MAX_TOKENS"}),
                ),
                (
                    "messageStopEvent",
                    json!({"messageStopEvent":{"stopReason":"END_TURN"}}),
                ),
                ("contextUsageEvent", json!({"contextUsagePercentage":100.0})),
            ]);
            let (provider, server) = loopback_provider(body).await;
            let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
            let state = AppState::new(false, ToolCompatibilityMode::ClaudeCode)
                .with_kiro_provider(provider)
                .with_usage(None, None, Some(usage.clone()));
            let response = invoke(entry, state, streaming, false).await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            server.await.unwrap();
            let output = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(
                output.contains(if entry % 4 < 2 {
                    "refusal"
                } else {
                    "content_filter"
                }),
                "{output}"
            );
            assert_eq!(
                (
                    usage.overview().today_calls,
                    usage.overview().today_errors,
                    usage.overview().today_input_tokens,
                    usage.overview().today_output_tokens
                ),
                (1, 0, 3, 17),
                "entry={entry}, stream={streaming}: {output}"
            );
        }
    }
}

#[tokio::test]
async fn event_compatibility_first_frame_exceptions_keep_status_and_single_error_record() {
    use super::super::test_support::event_frame;
    for (name, status) in [
        ("ThrottlingException", 429),
        ("ServiceQuotaExceededException", 429),
        ("UnauthorizedException", 401),
        ("AccessDeniedException", 403),
        ("ValidationException", 400),
        ("InvalidModelException", 400),
        ("ResourceNotFoundException", 404),
        ("ConflictException", 409),
        ("InternalServerException", 502),
        ("ServiceUnavailableException", 502),
        ("BrandNewException", 502),
    ] {
        for entry in 0..4 {
            for streaming in [false, true] {
                let body = event_frame(
                    &[(":message-type", "exception"), (":exception-type", name)],
                    &json!({"message":"upstream failure"}),
                );
                let (provider, server) = loopback_provider(body).await;
                let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
                let state = AppState::new(false, ToolCompatibilityMode::ClaudeCode)
                    .with_kiro_provider(provider)
                    .with_usage(None, None, Some(usage.clone()));
                let response = invoke(entry, state, streaming, false).await;
                assert_eq!(
                    response.status().as_u16(),
                    if streaming { 200 } else { status },
                    "{name}"
                );
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                server.await.unwrap();
                let output = String::from_utf8(bytes.to_vec()).unwrap();
                assert!(output.contains("\"error\""), "{name}: {output}");
                assert!(!output.contains("event: message_stop"), "{output}");
                assert!(!output.contains("\"status\":\"completed\""), "{output}");
                assert_eq!(
                    (usage.overview().today_calls, usage.overview().today_errors),
                    (1, 1)
                );
            }
        }
    }
}

#[tokio::test]
async fn explicit_thinking_disable_hides_native_and_legacy_on_all_entry_points() {
    for entry in 0..8 {
        for streaming in [false, true] {
            let body = event_stream(&[
                (
                    "reasoningContentEvent",
                    json!({"reasoningText":{"text":"SECRET_NESTED"},"signature":"SECRET_NESTED_SIGNATURE"}),
                ),
                (
                    "reasoningContentEvent",
                    json!({"text":"SECRET_NATIVE","signature":"SECRET_SIGNATURE","redactedContent":"SECRET_REDACTED"}),
                ),
                (
                    "assistantResponseEvent",
                    json!({"content":"<thinking>SECRET_LEGACY</thinking>\n\nanswer"}),
                ),
                (
                    "metadataEvent",
                    json!({"tokenUsage":{"uncachedInputTokens":3,"outputTokens":17}}),
                ),
            ]);
            let (provider, server) = loopback_provider(body).await;
            let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
            let state = AppState::new(false, ToolCompatibilityMode::ClaudeCode)
                .with_kiro_provider(provider)
                .with_usage(None, None, Some(usage.clone()));
            let response = invoke(entry, state, streaming, true).await;
            assert_eq!(response.status(), StatusCode::OK, "entry={entry}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            server.await.unwrap();
            let output = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(
                !output.contains("SECRET_"),
                "entry={entry}, stream={streaming}: {output}"
            );
            assert!(output.contains("answer"), "{output}");
            assert_eq!(
                (
                    usage.overview().today_calls,
                    usage.overview().today_errors,
                    usage.overview().today_output_tokens
                ),
                (1, 0, 17)
            );
        }
    }
}

#[tokio::test]
async fn tool_arguments_reach_every_streaming_entry_before_upstream_completion() {
    for entry in 0..8 {
        let first = event_stream(&[(
            "toolUseEvent",
            json!({"toolUseId":"call1","name":"custom_tool","input":"{\"x\":","stop":false}),
        )]);
        let last = event_stream(&[
            (
                "toolUseEvent",
                json!({"toolUseId":"call1","input":"1}","stop":true}),
            ),
            (
                "metadataEvent",
                json!({"tokenUsage":{"uncachedInputTokens":3,"outputTokens":11}}),
            ),
        ]);
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let wait = Arc::new(tokio::sync::Mutex::new(Some(wait)));
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let (first, last, wait) = (first.clone(), last.clone(), wait.clone());
                async move {
                    let wait = wait.lock().await.take().unwrap();
                    Body::from_stream(
                        stream::once(async move { Ok::<_, Infallible>(Bytes::from(first)) }).chain(
                            stream::once(async move {
                                wait.await.unwrap();
                                Ok::<_, Infallible>(Bytes::from(last))
                            }),
                        ),
                    )
                }
            }),
        );
        let (url, server) = serve(app).await;
        let provider = provider_for_url(url, Config::default());
        let state =
            AppState::new(true, ToolCompatibilityMode::ClaudeCode).with_kiro_provider(provider);
        let response = invoke(entry, state, true, false).await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let mut early = String::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !early.contains("{\\\"x\\\":") {
                let chunk = body.next().await.unwrap().unwrap();
                early.push_str(&String::from_utf8_lossy(&chunk));
            }
        })
        .await
        .expect("tool fragment must arrive while upstream is gated");
        assert!(!early.contains("message_stop") && !early.contains("response.completed"));
        release.send(()).unwrap();
        let rest = tokio::time::timeout(Duration::from_secs(2), async {
            let mut output = String::new();
            while let Some(chunk) = body.next().await {
                output.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            }
            output
        })
        .await
        .unwrap();
        assert!(!rest.contains("error"), "{rest}");
        assert!(rest.contains(if entry % 4 == 3 {
            "response.completed"
        } else if entry % 4 == 2 {
            "[DONE]"
        } else {
            "message_stop"
        }));
        let id_mentions = format!("{early}{rest}").matches("\"id\":\"call1\"").count();
        if entry % 4 < 2 {
            assert_eq!(id_mentions, 1, "never emit the same tool twice");
        }
        server.abort();
    }
}

#[tokio::test]
async fn api_requests_reuse_the_same_connection() {
    use axum::extract::ConnectInfo;
    let peers = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let captured = peers.clone();
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(
            move |ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
                  headers: header::HeaderMap,
                  _: Bytes| {
                captured.lock().push(peer);
                assert_ne!(
                    headers
                        .get(header::CONNECTION)
                        .and_then(|value| value.to_str().ok()),
                    Some("close")
                );
                async { "ok" }
            },
        ),
    );
    let (url, server) = serve(app).await;
    let provider = provider_for_url(url, Config::default());
    for i in 0..3 {
        let response = if i == 1 {
            provider.call_mcp("{}", None).await.unwrap()
        } else {
            provider.call_api("{}", None, None).await.unwrap().response
        };
        assert_eq!(response.text().await.unwrap(), "ok");
    }
    let peers = peers.lock();
    assert_eq!(peers.len(), 3);
    assert!(
        peers.iter().all(|peer| peer == &peers[0]),
        "sequential calls must reuse one TCP connection: {peers:?}"
    );
    server.abort();
}

#[tokio::test]
async fn refused_partial_tools_fail_safely_and_settle_once_on_every_streaming_entry() {
    for entry in 0..8 {
        let (provider, server) = loopback_provider(event_stream(&[
            ("toolUseEvent", json!({"toolUseId":"partial","name":"custom_tool","input":"{\"x\":","stop":false})),
            ("metadataEvent", json!({"stopReason":"CONTENT_FILTERED","tokenUsage":{"uncachedInputTokens":3,"outputTokens":17}})),
        ])).await;
        let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
        let state = AppState::new(true, ToolCompatibilityMode::ClaudeCode)
            .with_kiro_provider(provider)
            .with_usage(None, None, Some(usage.clone()));
        let response = invoke(entry, state, true, false).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        server.await.unwrap();
        let output = String::from_utf8(body.to_vec()).unwrap();
        assert!(output.contains("error"), "{output}");
        assert!(
            !output.contains("message_stop")
                && !output.contains("response.completed")
                && !output.contains("response.function_call_arguments.done")
                && !output.contains("event: content_block_stop")
                && !output.contains("\"finish_reason\":\"tool_calls\""),
            "{output}"
        );
        if entry % 4 == 3 {
            assert!(output.contains("response.failed"));
        }
        assert_eq!(
            (
                usage.overview().today_calls,
                usage.overview().today_errors,
                usage.overview().today_output_tokens
            ),
            (1, 1, 17)
        );
    }
}

#[tokio::test]
async fn response_budget_bounds_retries_and_settles_timeout_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move || {
            let calls = captured.clone();
            async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                } else {
                    futures::future::pending::<Response>().await
                }
            }
        }),
    );
    let (url, server) = serve(app).await;
    let mut config = Config::default();
    config.upstream_first_response_timeout_secs = 1;
    config.upstream_read_timeout_secs = 5;
    let provider = provider_for_url(url, config);
    let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
    let state = AppState::new(true, ToolCompatibilityMode::Raw)
        .with_kiro_provider(provider.clone())
        .with_usage(None, None, Some(usage.clone()));
    let started = Instant::now();
    let response = tokio::time::timeout(Duration::from_secs(3), invoke(0, state, true, false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(
        started.elapsed() >= Duration::from_millis(900)
            && started.elapsed() < Duration::from_millis(1700)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        (usage.overview().today_calls, usage.overview().today_errors),
        (1, 1)
    );
    let started = Instant::now();
    let error = match provider.call_mcp("{}", None).await {
        Err(error) => error,
        Ok(_) => panic!("MCP must time out"),
    };
    assert!(
        error
            .downcast_ref::<crate::kiro::error::UpstreamTimeoutError>()
            .is_some()
    );
    assert!(started.elapsed() < Duration::from_millis(1700));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    server.abort();
}

#[tokio::test]
async fn upstream_idle_timeout_does_not_complete_or_replay_partial_tools() {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    let first = event_stream(&[(
        "toolUseEvent",
        json!({"toolUseId":"call1","name":"custom_tool","input":"{\"x\":","stop":false}),
    )]);
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move || {
            captured.fetch_add(1, Ordering::SeqCst);
            let first = first.clone();
            async move {
                Body::from_stream(
                    stream::once(async move { Ok::<_, Infallible>(Bytes::from(first)) })
                        .chain(stream::pending()),
                )
            }
        }),
    );
    let (url, server) = serve(app).await;
    let mut config = Config::default();
    config.upstream_read_timeout_secs = 1;
    let provider = provider_for_url(url, config);
    let usage = Arc::new(crate::admin::usage_stats::UsageAggregator::new());
    let state = AppState::new(true, ToolCompatibilityMode::Raw)
        .with_kiro_provider(provider)
        .with_usage(None, None, Some(usage.clone()));
    let response = invoke(1, state, true, false).await;
    let body = tokio::time::timeout(
        Duration::from_secs(3),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .unwrap()
    .unwrap();
    let output = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        output.contains("input_json_delta") && output.contains("timeout_error"),
        "{output}"
    );
    assert!(
        !output.contains("message_stop")
            && !output.contains("message_delta")
            && !output.contains("content_block_stop"),
        "{output}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "never replay after output");
    assert_eq!(
        (usage.overview().today_calls, usage.overview().today_errors),
        (1, 1)
    );
    server.abort();
}

#[tokio::test]
async fn total_deadline_covers_retry_wait_and_body_instead_of_resetting() {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = calls.clone();
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move || {
            let first = captured.fetch_add(1, Ordering::SeqCst) == 0;
            async move {
                if first {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                } else {
                    Body::from_stream(stream::unfold(0, |i| async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        Some((Ok::<_, Infallible>(Bytes::from_static(b"x")), i + 1))
                    }))
                    .into_response()
                }
            }
        }),
    );
    let (url, server) = serve(app).await;
    let mut config = Config::default();
    config.upstream_request_timeout_secs = 2;
    config.upstream_read_timeout_secs = 1;
    let provider = provider_for_url(url, config);
    let started = Instant::now();
    let response = provider.call_api("{}", None, None).await.unwrap().response;
    let error = tokio::time::timeout(Duration::from_secs(3), response.bytes())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.is_timeout());
    assert!(
        started.elapsed() >= Duration::from_millis(1900)
            && started.elapsed() < Duration::from_millis(2400),
        "total elapsed: {:?}",
        started.elapsed()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
}
