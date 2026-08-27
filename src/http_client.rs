//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, Proxy, redirect::Policy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 代理配置
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    fn first_proxy_url(&self) -> Option<String> {
        Self::split_candidates(&self.url)
            .into_iter()
            .next()
            .filter(|candidate| !Self::is_direct(candidate))
    }

    /// `direct` 表示显式直连。代理列表里也允许把它作为兜底候选。
    pub fn is_direct(value: &str) -> bool {
        value.trim().eq_ignore_ascii_case("direct")
    }

    /// 将逗号/空白/换行分隔的代理字符串拆成候选项，保留 `direct`。
    pub fn split_candidates(raw: &str) -> Vec<String> {
        raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .fold(Vec::new(), |mut acc, item| {
                if !acc
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(item))
                {
                    acc.push(item.to_string());
                }
                acc
            })
    }

    /// 单个配置是否是合法代理 URL 或 direct。
    pub fn is_supported_entry(value: &str) -> bool {
        let value = value.trim();
        Self::is_direct(value)
            || value.starts_with("http://")
            || value.starts_with("https://")
            || value.starts_with("socks5://")
            || value.starts_with("socks4://")
    }

    /// 与数据面故障转移同口径：仅这些 HTTP 状态值得换下一个代理候选。
    pub fn should_try_next_proxy_status(status: u16) -> bool {
        matches!(status, 407 | 502 | 503 | 504)
    }

    pub fn from_url_with_auth(
        url: impl Into<String>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Option<Self> {
        let url = url.into();
        if Self::is_direct(&url) {
            return None;
        }
        let mut proxy = Self::new(url);
        if let (Some(username), Some(password)) = (username, password) {
            if !username.is_empty() || !password.is_empty() {
                proxy = proxy.with_auth(username, password);
            }
        }
        Some(proxy)
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    build_client_with_redirect_policy(proxy, timeout_secs, tls_backend, None)
}

pub fn build_client_no_redirect(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    build_client_with_redirect_policy(proxy, timeout_secs, tls_backend, Some(Policy::none()))
}

fn build_client_with_redirect_policy(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
    redirect_policy: Option<Policy>,
) -> anyhow::Result<Client> {
    let mut builder = Client::builder().timeout(Duration::from_secs(timeout_secs));
    if let Some(policy) = redirect_policy {
        builder = builder.redirect(policy);
    }

    match tls_backend {
        TlsBackend::Rustls => {
            builder = builder.use_rustls_tls();
        }
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            {
                anyhow::bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
            }
        }
    }

    if let Some(proxy_config) = proxy {
        let Some(proxy_url) = proxy_config.first_proxy_url() else {
            return Ok(builder.build()?);
        };
        let mut proxy = Proxy::all(&proxy_url)?;

        // 设置代理认证
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(username, password);
        }

        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 使用代理: {}", proxy_url);
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_uses_first_non_direct_candidate() {
        let config = ProxyConfig::new("http://127.0.0.1:7890, direct");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_should_try_next_proxy_status() {
        assert!(ProxyConfig::should_try_next_proxy_status(407));
        assert!(ProxyConfig::should_try_next_proxy_status(502));
        assert!(ProxyConfig::should_try_next_proxy_status(503));
        assert!(ProxyConfig::should_try_next_proxy_status(504));
        assert!(!ProxyConfig::should_try_next_proxy_status(401));
        assert!(!ProxyConfig::should_try_next_proxy_status(403));
        assert!(!ProxyConfig::should_try_next_proxy_status(429));
        assert!(!ProxyConfig::should_try_next_proxy_status(400));
    }

    #[test]
    fn test_split_proxy_candidates() {
        let candidates = ProxyConfig::split_candidates(
            "socks5://a:1080, http://b:8080\ndirect  socks5://a:1080",
        );
        assert_eq!(
            candidates,
            vec![
                "socks5://a:1080".to_string(),
                "http://b:8080".to_string(),
                "direct".to_string(),
            ]
        );
    }
}
