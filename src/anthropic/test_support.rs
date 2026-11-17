use std::{collections::HashMap, sync::Arc};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::kiro::{
    endpoint::{KiroEndpoint, RequestContext},
    model::credentials::KiroCredentials,
    parser::crc::crc32,
    provider::KiroProvider,
    token_manager::MultiTokenManager,
};
use crate::model::config::Config;

pub(super) fn event_stream(events: &[(&str, Value)]) -> Vec<u8> {
    let mut output = Vec::new();
    for (event, payload) in events {
        let mut headers = Vec::new();
        for (name, value) in [(":message-type", "event"), (":event-type", *event)] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let payload = serde_json::to_vec(payload).unwrap();
        let mut frame = Vec::new();
        frame.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        output.extend(frame);
    }
    output
}

struct LoopbackEndpoint(String);

impl KiroEndpoint for LoopbackEndpoint {
    fn name(&self) -> &'static str {
        "test"
    }
    fn api_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn mcp_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn decorate_api(
        &self,
        request: reqwest::RequestBuilder,
        _: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        request
    }
    fn decorate_mcp(
        &self,
        request: reqwest::RequestBuilder,
        _: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        request
    }
    fn transform_api_body(&self, body: &str, _: &RequestContext<'_>) -> String {
        body.to_string()
    }
}

/// 真实 provider/HTTP 入口，仅允许一次本地调用，额外模型轮次或 MCP 调用会让测试失败。
pub(super) async fn loopback_provider(
    body: Vec<u8>,
) -> (Arc<KiroProvider>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    let length = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
                    }).unwrap_or(0);
                    if request.len() >= end + 4 + length { break; }
                }
            }
            let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }).await.unwrap();
    });
    (provider_for_url(url, Config::default()), server)
}

pub(super) fn provider_for_url(url: String, config: Config) -> Arc<KiroProvider> {
    let credential = KiroCredentials {
        kiro_api_key: Some("test-key".into()),
        ..Default::default()
    };
    let manager =
        Arc::new(MultiTokenManager::new(config, vec![credential], None, None, false).unwrap());
    let endpoints = HashMap::from([(
        "test".into(),
        Arc::new(LoopbackEndpoint(url)) as Arc<dyn KiroEndpoint>,
    )]);
    Arc::new(KiroProvider::with_proxy(
        manager,
        None,
        endpoints,
        "test".into(),
        None,
    ))
}
