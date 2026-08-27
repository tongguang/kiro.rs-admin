//! Kiro OAuth 凭证数据模型
//!
//! 支持从 Kiro IDE 的凭证文件加载，使用 Social 认证方式
//! 支持单凭据和多凭据配置格式

use base64::{Engine, engine::general_purpose};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::http_client::ProxyConfig;
use crate::model::config::Config;

pub const BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";
pub const SOCIAL_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";

/// Kiro OAuth 凭证
///
/// `Debug` 输出经过脱敏处理：access_token / refresh_token / client_secret /
/// kiro_api_key / proxy_password 等敏感字段只显示长度，不会泄露明文。
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KiroCredentials {
    /// 凭据唯一标识符（自增 ID）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,

    /// 访问令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,

    /// 刷新令牌
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,

    /// Profile ARN
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,

    /// 过期时间 (RFC3339 格式)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,

    /// 认证方式 (social / idc / external_idp / api_key)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,

    /// 身份提供商（BuilderId / Enterprise / Github / Google / IAM_SSO / AzureAD）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    /// OIDC Client ID（IdC 认证需要；external_idp 刷新也需要）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// OIDC Client Secret (IdC 认证需要)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,

    /// SSO Start URL（Enterprise / IAM Identity Center 账号专用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,

    /// 企业 SSO (external_idp，如 Microsoft Entra ID / Azure AD) 的 OAuth2 token 端点。
    ///
    /// 当 `auth_method == "external_idp"` 时，Token 通过 public client refresh_token
    /// grant 打到该端点刷新，而非 AWS SSO OIDC 端点。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,

    /// 企业 SSO 的 OIDC issuer URL（端点的发现来源，纯记录用途）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,

    /// 企业 SSO 授予的 scopes（空格分隔）。刷新时作为 `scope` 参数回传，
    /// 其中的 `offline_access` 是拿到 refresh_token 的前提。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,

    /// 凭据优先级（数字越小优先级越高，默认为 0）
    #[serde(default)]
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: u32,

    /// 每分钟请求数上限（RPM 滑动窗口）。默认 10；0 表示不限速。
    /// 始终序列化（不 skip）：默认值 10 ≠ 0，若 skip 掉则用户显式设的 0（不限速）
    /// 会因缺字段被反序列化回默认 10，丢失「不限速」语义。
    #[serde(default = "default_rpm_limit")]
    pub rpm_limit: u32,

    /// 凭据级 Region 配置（用于 OIDC token 刷新）
    /// 未配置时回退到 config.json 的全局 region
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// 凭据级 Auth Region（用于 Token 刷新）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// 凭据级 API Region（用于 API 请求）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    /// 凭据级 Machine ID 配置（可选）
    /// 未配置时回退到 config.json 的 machineId；都未配置时由 refreshToken 派生
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,

    /// 用户邮箱（从 Anthropic API 获取）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// 订阅等级（KIRO PRO+ / KIRO FREE 等）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub subscription_title: Option<String>,

    /// 凭据级代理 URL（可选）
    /// 支持 http/https/socks5 协议
    /// 特殊值 "direct" 表示显式不使用代理（即使全局配置了代理）
    /// 未配置时回退到全局代理配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,

    /// 凭据级代理认证用户名（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,

    /// 凭据级代理认证密码（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,

    /// 凭据是否被禁用（默认为 false）
    #[serde(default)]
    pub disabled: bool,

    /// 禁用原因。与 `disabled` 一起持久化，避免重启后把自动禁用误判为手动禁用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,

    /// 当前凭据连续执行自愈的轮数。成功调用后清零。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub self_heal_consecutive_rounds: u32,

    /// 当前凭据累计被自愈恢复的次数，仅用于观测。
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub self_heal_total_count: u64,

    /// 最近一次自愈时间（RFC3339）。用于让冷却窗口跨进程重启生效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_self_heal_at: Option<String>,

    /// 触发当前连续自愈轮次的模型。`None` 表示 MCP/无模型请求。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_heal_model: Option<String>,

    /// Kiro API Key（headless 模式）
    /// 格式: ksk_xxxxxxxx
    /// 设置后直接作为 Bearer Token 使用，无需 refreshToken
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kiro_api_key: Option<String>,

    /// 端点名称（可选）
    ///
    /// 决定该凭据走哪套 Kiro API。未配置时回退到 `config.defaultEndpoint`（默认 "ide"）。
    /// 端点名必须在启动时注册的端点 registry 中存在。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// 账号所属分组（可属于多个分组）
    ///
    /// 客户端 Key 绑定某个分组后，用该 Key 发起的请求只会调度到 groups 包含该分组名的账号。
    /// 空数组表示该账号不属于任何分组（仅未绑定分组的 Key / master apiKey 可使用）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,

    /// 账号来源渠道（纯备注）
    ///
    /// 标记该账号的购买来源/渠道，便于运营追踪。不参与调度、导出或筛选。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_channel: Option<String>,
}

/// 判断是否为零（用于跳过序列化）
fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// 判断是否为零（u64 版，用于跳过序列化）
fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// rpm_limit 缺省值：默认每分钟 10 次。
fn default_rpm_limit() -> u32 {
    10
}

/// 仅显示长度，不暴露明文。例如 `Some(42 chars)` 或 `None`。
fn fmt_redacted(value: &Option<String>) -> String {
    match value {
        Some(s) if !s.is_empty() => format!("Some({} chars)", s.chars().count()),
        Some(_) => "Some(<empty>)".to_string(),
        None => "None".to_string(),
    }
}

impl std::fmt::Debug for KiroCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 单独脱敏所有可能含密钥/Token 的字段，其他元数据正常打印
        f.debug_struct("KiroCredentials")
            .field("id", &self.id)
            .field("access_token", &fmt_redacted(&self.access_token))
            .field("refresh_token", &fmt_redacted(&self.refresh_token))
            .field("profile_arn", &self.profile_arn)
            .field("expires_at", &self.expires_at)
            .field("auth_method", &self.auth_method)
            .field("provider", &self.provider)
            .field("client_id", &fmt_redacted(&self.client_id))
            .field("client_secret", &fmt_redacted(&self.client_secret))
            .field("start_url", &self.start_url)
            .field("token_endpoint", &self.token_endpoint)
            .field("issuer_url", &self.issuer_url)
            .field("scopes", &self.scopes)
            .field("priority", &self.priority)
            .field("rpm_limit", &self.rpm_limit)
            .field("region", &self.region)
            .field("auth_region", &self.auth_region)
            .field("api_region", &self.api_region)
            .field("machine_id", &fmt_redacted(&self.machine_id))
            .field("email", &self.email)
            .field("subscription_title", &self.subscription_title)
            .field("proxy_url", &self.proxy_url)
            .field("proxy_username", &self.proxy_username)
            .field("proxy_password", &fmt_redacted(&self.proxy_password))
            .field("disabled", &self.disabled)
            .field("disabled_reason", &self.disabled_reason)
            .field(
                "self_heal_consecutive_rounds",
                &self.self_heal_consecutive_rounds,
            )
            .field("self_heal_total_count", &self.self_heal_total_count)
            .field("last_self_heal_at", &self.last_self_heal_at)
            .field("self_heal_model", &self.self_heal_model)
            .field("kiro_api_key", &fmt_redacted(&self.kiro_api_key))
            .field("endpoint", &self.endpoint)
            .field("groups", &self.groups)
            .field("source_channel", &self.source_channel)
            .finish()
    }
}

/// 企业 SSO (external_idp) 的 auth_method 别名。凭据来源多样（Kiro 导出、Azure 门户、
/// 手工），统一归一到规范值 `external_idp`。
const EXTERNAL_IDP_ALIASES: &[&str] = &[
    "external_idp",
    "azuread",
    "azure",
    "entra",
    "entra-id",
    "microsoft",
    "m365",
    "office365",
    "external",
];

pub(crate) fn canonicalize_auth_method_value(value: &str) -> &str {
    if value.eq_ignore_ascii_case("builder-id") || value.eq_ignore_ascii_case("iam") {
        "idc"
    } else if value.eq_ignore_ascii_case("api_key") || value.eq_ignore_ascii_case("apikey") {
        "api_key"
    } else if EXTERNAL_IDP_ALIASES
        .iter()
        .any(|a| value.eq_ignore_ascii_case(a))
    {
        "external_idp"
    } else {
        value
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ExternalIdpImportFields<'a> {
    pub auth_method: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub idp: Option<&'a str>,
    pub token_endpoint: Option<&'a str>,
    pub issuer_url: Option<&'a str>,
    pub scopes: Option<&'a str>,
    pub user_id: Option<&'a str>,
    pub access_token: Option<&'a str>,
    pub client_id: Option<&'a str>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CompletedExternalIdpImportFields {
    pub token_endpoint: Option<String>,
    pub issuer_url: Option<String>,
    pub scopes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MicrosoftIdpTenant {
    host: String,
    tenant: String,
}

fn clean_import_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn is_external_idp_alias(value: Option<&str>) -> bool {
    value
        .and_then(|v| clean_import_string(Some(v)))
        .is_some_and(|v| {
            EXTERNAL_IDP_ALIASES
                .iter()
                .any(|a| v.eq_ignore_ascii_case(a))
        })
}

fn extract_microsoft_idp_tenant(raw: Option<&str>) -> Option<MicrosoftIdpTenant> {
    let value = clean_import_string(raw)?;
    let url = reqwest::Url::parse(&value).ok()?;
    if !url.scheme().eq_ignore_ascii_case("https") {
        return None;
    }

    let host = url.host_str()?.to_ascii_lowercase();
    let login_host = if host == "sts.windows.net" {
        "login.microsoftonline.com".to_string()
    } else if ALLOWED_EXTERNAL_IDP_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix))
    {
        host
    } else {
        return None;
    };

    let tenant = url
        .path_segments()
        .and_then(|mut segments| segments.find(|segment| !segment.trim().is_empty()))?;
    if tenant.eq_ignore_ascii_case("oauth2") || tenant.eq_ignore_ascii_case("v2.0") {
        return None;
    }

    Some(MicrosoftIdpTenant {
        host: login_host,
        tenant: tenant.to_string(),
    })
}

fn default_external_idp_scopes(client_id: Option<&str>) -> Option<String> {
    let client_id = clean_import_string(client_id)?;
    Some(
        [
            format!("api://{client_id}/codewhisperer:conversations"),
            format!("api://{client_id}/codewhisperer:completions"),
            "offline_access".to_string(),
        ]
        .join(" "),
    )
}

fn normalize_external_idp_scopes(raw_scopes: &str, client_id: Option<&str>) -> Option<String> {
    let scopes: Vec<&str> = raw_scopes
        .split_whitespace()
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .collect();
    if scopes.is_empty() {
        return default_external_idp_scopes(client_id);
    }

    let client_id = clean_import_string(client_id);
    let mut normalized: Vec<String> = Vec::new();
    for scope in scopes {
        let next = if let Some(client_id) = client_id.as_deref() {
            if !scope.contains("://") && !scope.eq_ignore_ascii_case("offline_access") {
                format!("api://{}/{}", client_id, scope.trim_start_matches('/'))
            } else {
                scope.to_string()
            }
        } else {
            scope.to_string()
        };

        if !normalized
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&next))
        {
            normalized.push(next);
        }
    }

    if client_id.is_some()
        && !normalized
            .iter()
            .any(|scope| scope.eq_ignore_ascii_case("offline_access"))
    {
        normalized.push("offline_access".to_string());
    }

    (!normalized.is_empty()).then(|| normalized.join(" "))
}

fn decode_jwt_payload(access_token: Option<&str>) -> Option<serde_json::Value> {
    let token = clean_import_string(access_token)?;
    let payload = token.split('.').nth(1)?;
    let decoded = general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn derive_jwt_string_claim(access_token: Option<&str>, claim: &str) -> Option<String> {
    decode_jwt_payload(access_token)?
        .get(claim)?
        .as_str()
        .and_then(|value| clean_import_string(Some(value)))
}

fn derive_external_idp_scopes_from_access_token(
    access_token: Option<&str>,
    client_id: Option<&str>,
) -> Option<String> {
    let scp = derive_jwt_string_claim(access_token, "scp")?;
    normalize_external_idp_scopes(&scp, client_id)
}

fn is_external_idp_import_like(fields: ExternalIdpImportFields<'_>) -> bool {
    is_external_idp_alias(fields.auth_method)
        || is_external_idp_alias(fields.provider)
        || is_external_idp_alias(fields.idp)
        || clean_import_string(fields.token_endpoint).is_some()
        || extract_microsoft_idp_tenant(fields.issuer_url).is_some()
        || extract_microsoft_idp_tenant(fields.user_id).is_some()
        || extract_microsoft_idp_tenant(
            derive_jwt_string_claim(fields.access_token, "iss").as_deref(),
        )
        .is_some()
}

pub(crate) fn complete_external_idp_import_fields(
    fields: ExternalIdpImportFields<'_>,
) -> CompletedExternalIdpImportFields {
    let token_endpoint = clean_import_string(fields.token_endpoint);
    let issuer_url = clean_import_string(fields.issuer_url);
    let scopes = clean_import_string(fields.scopes)
        .and_then(|raw| normalize_external_idp_scopes(&raw, fields.client_id));

    if !is_external_idp_import_like(fields) {
        return CompletedExternalIdpImportFields {
            token_endpoint,
            issuer_url,
            scopes,
        };
    }

    let tenant = extract_microsoft_idp_tenant(token_endpoint.as_deref())
        .or_else(|| extract_microsoft_idp_tenant(issuer_url.as_deref()))
        .or_else(|| extract_microsoft_idp_tenant(fields.user_id))
        .or_else(|| {
            extract_microsoft_idp_tenant(
                derive_jwt_string_claim(fields.access_token, "iss").as_deref(),
            )
        });

    CompletedExternalIdpImportFields {
        token_endpoint: token_endpoint.or_else(|| {
            tenant.as_ref().map(|tenant| {
                format!(
                    "https://{}/{}/oauth2/v2.0/token",
                    tenant.host, tenant.tenant
                )
            })
        }),
        issuer_url: issuer_url.or_else(|| {
            tenant
                .as_ref()
                .map(|tenant| format!("https://{}/{}/v2.0", tenant.host, tenant.tenant))
        }),
        scopes: scopes
            .or_else(|| {
                derive_external_idp_scopes_from_access_token(fields.access_token, fields.client_id)
            })
            .or_else(|| default_external_idp_scopes(fields.client_id)),
    }
}

pub(crate) fn normalize_import_auth_method_from_fields(
    fields: ExternalIdpImportFields<'_>,
) -> String {
    let canonical = canonicalize_auth_method_value(fields.auth_method.unwrap_or("social").trim());
    if canonical.eq_ignore_ascii_case("external_idp") || is_external_idp_import_like(fields) {
        return "external_idp".to_string();
    }
    canonical.to_string()
}

/// 导入路径的 auth_method 归一化。
///
/// 在别名规范化之外，额外做一步推断：若显式声明的方式不是企业 SSO，但携带了
/// `tokenEndpoint`（social/idc 均无此字段），则判定为 `external_idp`。这样即便粘贴的
/// JSON 未写 authMethod，只要带 tokenEndpoint 就能被正确识别。
#[allow(dead_code)]
pub(crate) fn normalize_import_auth_method(raw: &str, token_endpoint: Option<&str>) -> String {
    let canonical = canonicalize_auth_method_value(raw.trim());
    if canonical.eq_ignore_ascii_case("external_idp") {
        return "external_idp".to_string();
    }
    if token_endpoint.is_some_and(|e| !e.trim().is_empty()) {
        return "external_idp".to_string();
    }
    canonical.to_string()
}

/// 企业 SSO IdP 端点允许列表（后缀锚定）。
///
/// `tokenEndpoint` 是外发 refreshToken 的目标，属新的信任边界；导入的凭据可能来自不可信
/// 来源（如共享账号包），若指向内网/攻击者控制的主机会导致 refreshToken 泄露。故限制到
/// 已知企业 IdP 主机（Microsoft Entra / Azure AD）。前导点锚定到真实子域边界，
/// `evil-microsoftonline.com` 无法命中。新增其它 IdP 时扩展此列表。
pub const ALLOWED_EXTERNAL_IDP_SUFFIXES: &[&str] = &[
    ".microsoftonline.com",
    ".microsoftonline.us",
    ".microsoftonline.cn",
];

/// 校验企业 SSO IdP 端点 URL 是否可安全外发。
///
/// 要求：可解析、必须 https、host 非 IP 字面量、host 命中 [`ALLOWED_EXTERNAL_IDP_SUFFIXES`]。
/// 用于 Token 刷新（外发 refreshToken 前）与导入校验两处，防 SSRF / 凭据外泄。
pub fn validate_external_idp_endpoint(raw_url: &str) -> Result<(), String> {
    let url =
        reqwest::Url::parse(raw_url.trim()).map_err(|e| format!("IdP 端点 URL 无法解析: {}", e))?;

    if !url.scheme().eq_ignore_ascii_case("https") {
        return Err("IdP 端点 URL 必须为 https".to_string());
    }

    let host = match url.host_str() {
        Some(h) if !h.is_empty() => h.to_ascii_lowercase(),
        _ => return Err("IdP 端点 URL 缺少 host".to_string()),
    };

    // 拒绝 IP 字面量（含 IPv6，url 的 host_str 对 IPv6 返回不带方括号的形式）
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err("IdP 端点 host 不能是 IP 字面量".to_string());
    }

    if ALLOWED_EXTERNAL_IDP_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix))
    {
        Ok(())
    } else {
        Err(format!("IdP 端点 host {:?} 不在允许列表内", host))
    }
}

/// 凭据配置（支持单对象或数组格式）
///
/// 自动识别配置文件格式：
/// - 单对象格式（旧格式，向后兼容）
/// - 数组格式（新格式，支持多凭据）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CredentialsConfig {
    /// 单个凭据（旧格式）
    Single(KiroCredentials),
    /// 多凭据数组（新格式）
    Multiple(Vec<KiroCredentials>),
}

impl CredentialsConfig {
    /// 从文件加载凭据配置
    ///
    /// - 如果文件不存在，返回空数组
    /// - 如果文件内容为空，返回空数组
    /// - 支持单对象或数组格式
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();

        // 文件不存在时返回空数组
        if !path.exists() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let content = fs::read_to_string(path)?;

        // 文件为空时返回空数组
        if content.trim().is_empty() {
            return Ok(CredentialsConfig::Multiple(vec![]));
        }

        let config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// 转换为按优先级排序的凭据列表
    pub fn into_sorted_credentials(self) -> Vec<KiroCredentials> {
        match self {
            CredentialsConfig::Single(mut cred) => {
                cred.canonicalize_auth_method();
                vec![cred]
            }
            CredentialsConfig::Multiple(mut creds) => {
                // 按优先级排序（数字越小优先级越高）
                creds.sort_by_key(|c| c.priority);
                for cred in &mut creds {
                    cred.canonicalize_auth_method();
                }
                creds
            }
        }
    }

    /// 判断是否为多凭据格式（数组格式）
    pub fn is_multiple(&self) -> bool {
        matches!(self, CredentialsConfig::Multiple(_))
    }
}

impl KiroCredentials {
    /// 获取默认凭证文件路径
    pub fn default_credentials_path() -> &'static str {
        "credentials.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    pub fn effective_auth_region<'a>(&'a self, config: &'a Config) -> &'a str {
        self.auth_region
            .as_deref()
            .or(self.region.as_deref())
            .unwrap_or(config.effective_auth_region())
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先级：凭据.api_region > config.api_region > config.region
    pub fn effective_api_region<'a>(&'a self, config: &'a Config) -> &'a str {
        self.api_region
            .as_deref()
            .unwrap_or(config.effective_api_region())
    }

    /// 获取有效代理候选列表。
    ///
    /// `proxy_url` 可为单个 URL，也可用逗号/空白/换行分隔多个 URL；`direct` 会作为直连候选。
    /// 凭据未配置时继承全局候选；凭据显式配置时不再追加全局候选。
    pub fn effective_proxy_candidates(
        &self,
        global_proxies: &[Option<ProxyConfig>],
    ) -> Vec<Option<ProxyConfig>> {
        let own = self
            .proxy_url
            .as_deref()
            .map(ProxyConfig::split_candidates)
            .unwrap_or_default();

        if own.is_empty() {
            return if global_proxies.is_empty() {
                vec![None]
            } else {
                global_proxies.to_vec()
            };
        }

        let mut out = Vec::new();
        for candidate in own {
            if !ProxyConfig::is_supported_entry(&candidate) {
                continue;
            }
            let next = ProxyConfig::from_url_with_auth(
                candidate,
                self.proxy_username.as_deref(),
                self.proxy_password.as_deref(),
            );
            if !out.iter().any(|existing| existing == &next) {
                out.push(next);
            }
        }

        if out.is_empty() { vec![None] } else { out }
    }

    pub fn canonicalize_auth_method(&mut self) {
        let auth_method = match &self.auth_method {
            Some(m) => m,
            None => return,
        };

        let canonical = canonicalize_auth_method_value(auth_method);
        if canonical != auth_method {
            self.auth_method = Some(canonical.to_string());
        }
    }

    pub fn fill_default_profile_arn(&mut self) -> bool {
        if self.profile_arn.is_some() || self.is_api_key_credential() {
            return false;
        }

        self.profile_arn = Some(self.default_profile_arn().to_string());
        true
    }

    /// 是否为 Social 登录（Github / Google）。
    fn is_social_login(&self) -> bool {
        self.auth_method
            .as_deref()
            .map(|m| m.eq_ignore_ascii_case("social"))
            .unwrap_or(false)
            || self
                .provider
                .as_deref()
                .map(|p| p.eq_ignore_ascii_case("github") || p.eq_ignore_ascii_case("google"))
                .unwrap_or(false)
    }

    /// 是否为外部 IdP（Microsoft Entra ID / Azure AD）企业 SSO 凭据。
    ///
    /// 这类账号走 OAuth2 公共客户端 `refresh_token` grant 刷新（见
    /// [`crate::kiro::token_manager`] 的 `refresh_external_idp_token`），且数据面 /
    /// Profile 请求必须携带 `TokenType: EXTERNAL_IDP` 头才能被 CodeWhisperer 识别。
    /// 与 [`Self::is_api_key_credential`] 互斥。
    pub fn is_external_idp(&self) -> bool {
        self.is_external_idp_credential()
    }

    /// 凭据缺少显式 profileArn 时应使用的默认 ARN：
    /// Social 登录用共享 Social ARN，其余（BuilderID 等）用 BuilderID 占位符。
    fn default_profile_arn(&self) -> &'static str {
        if self.is_social_login() {
            SOCIAL_PROFILE_ARN
        } else {
            BUILDER_ID_PROFILE_ARN
        }
    }

    /// 检查凭据是否支持 Opus 模型
    ///
    /// Free 账号不支持 Opus 模型，需要 PRO 或更高等级订阅
    pub fn supports_opus(&self) -> bool {
        match &self.subscription_title {
            Some(title) => {
                let title_upper = title.to_uppercase();
                // 如果包含 FREE，则不支持 Opus
                !title_upper.contains("FREE")
            }
            // 如果还没有获取订阅信息，暂时允许（首次使用时会获取）
            None => true,
        }
    }

    /// 检查是否为 API Key 凭据
    ///
    /// API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需 refreshToken
    pub fn is_api_key_credential(&self) -> bool {
        self.kiro_api_key.is_some()
            || self
                .auth_method
                .as_deref()
                .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                .unwrap_or(false)
    }

    /// 是否为企业 SSO (external_idp) 凭据。
    ///
    /// 容忍未规范化的别名（azuread/entra/... ），统一按规范值判断。
    pub fn is_external_idp_credential(&self) -> bool {
        self.auth_method
            .as_deref()
            .map(|m| canonicalize_auth_method_value(m).eq_ignore_ascii_case("external_idp"))
            .unwrap_or(false)
    }

    /// 返回该凭据在 CodeWhisperer 调用上应携带的 `tokentype` 头值（无则 None）。
    ///
    /// - API Key 凭据 → `"API_KEY"`
    /// - 企业 SSO 凭据 → `"EXTERNAL_IDP"`（缺此头上游会静默返回空 profile 列表并拒绝数据面调用）
    /// - social / idc → None
    pub fn token_type_header(&self) -> Option<&'static str> {
        if self.is_api_key_credential() {
            Some("API_KEY")
        } else if self.is_external_idp_credential() {
            Some("EXTERNAL_IDP")
        } else {
            None
        }
    }

    /// 返回「可发送给上游」的真实 profileArn（跳过 BuilderID 占位符）。
    ///
    /// - 真实 ARN（含 Social 共享 ARN）→ 原样返回；
    /// - [`BUILDER_ID_PROFILE_ARN`] 占位符 → 返回 `None`（非流式/头部类调用不应发送
    ///   BuilderID 占位符；流式请求请使用 [`Self::streaming_profile_arn`]）。
    pub fn effective_profile_arn(&self) -> Option<&str> {
        match self.profile_arn.as_deref() {
            Some(arn) if !is_placeholder_profile_arn(arn) => Some(arn),
            _ => None,
        }
    }

    /// 返回流式聊天端点（`generateAssistantResponse` / `SendMessageStreaming`）
    /// 应发送的 profileArn。
    ///
    /// 新版上游对流式端点强制要求 `profileArn`，缺失会返回
    /// `400 {"message":"profileArn is required for this request."}`。Enterprise/IdC
    /// 账号的真实 ARN 会先由 `resolve_profile_arn_for` 回填；纯 BuilderID 账号没有
    /// 可解析的真实 profile，按官方 IDE 行为发送 BuilderID 占位符。
    ///
    /// - 已有显式 profileArn（真实 ARN / Social ARN / BuilderID 占位符）→ 原样返回；
    /// - 尚未填充 → 按登录方式推断默认 ARN（Social → Social ARN，其余 → BuilderID）；
    /// - API Key 凭据无 profileArn 概念 → 返回 `None`。
    pub fn streaming_profile_arn(&self) -> Option<String> {
        if self.is_api_key_credential() {
            return None;
        }
        Some(
            self.profile_arn
                .clone()
                .unwrap_or_else(|| self.default_profile_arn().to_string()),
        )
    }
}

/// 判断给定 profileArn 是否为 BuilderID 占位符（非真实可用的 profile）。
pub fn is_placeholder_profile_arn(arn: &str) -> bool {
    arn == BUILDER_ID_PROFILE_ARN
}

#[cfg(test)]
impl KiroCredentials {
    fn from_json(json_string: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json_string)
    }

    fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Config;

    #[test]
    fn test_from_json() {
        let json = r#"{
            "accessToken": "test_token",
            "refreshToken": "test_refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2024-01-01T00:00:00Z",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2024-01-01T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("social".to_string()));
    }

    #[test]
    fn test_rpm_limit_serde_roundtrip() {
        // 1. 缺 rpmLimit 键的旧数据 → 回退到默认 10
        let legacy = r#"{ "accessToken": "t", "authMethod": "social" }"#;
        let c = KiroCredentials::from_json(legacy).unwrap();
        assert_eq!(c.rpm_limit, 10, "缺字段应回退默认 10");

        // 2. 显式 0（不限速）必须原样保留，不被吞回 10
        let zero = r#"{ "accessToken": "t", "rpmLimit": 0 }"#;
        let c0 = KiroCredentials::from_json(zero).unwrap();
        assert_eq!(c0.rpm_limit, 0, "显式 0 不应被默认覆盖");
        let json0 = c0.to_pretty_json().unwrap();
        assert!(json0.contains("rpmLimit"), "0 也必须序列化");
        assert_eq!(
            KiroCredentials::from_json(&json0).unwrap().rpm_limit,
            0,
            "0 序列化再读回仍为 0"
        );

        // 3. 自定义值往返
        let five = r#"{ "accessToken": "t", "rpmLimit": 5 }"#;
        assert_eq!(KiroCredentials::from_json(five).unwrap().rpm_limit, 5);
    }

    #[test]
    fn test_from_json_with_unknown_keys() {
        let json = r#"{
            "accessToken": "test_token",
            "unknownField": "should be ignored"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.access_token, Some("test_token".to_string()));
    }

    #[test]
    fn test_to_json() {
        let creds = KiroCredentials {
            id: None,
            access_token: Some("token".to_string()),
            refresh_token: None,
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            provider: None,
            client_id: None,
            client_secret: None,
            start_url: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: 0,
            rpm_limit: 10,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            disabled_reason: None,
            self_heal_consecutive_rounds: 0,
            self_heal_total_count: 0,
            last_self_heal_at: None,
            self_heal_model: None,
            kiro_api_key: None,
            endpoint: None,
            groups: vec![],
            source_channel: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("accessToken"));
        assert!(json.contains("authMethod"));
        assert!(!json.contains("refreshToken"));
        // priority 为 0 时不序列化
        assert!(!json.contains("priority"));
        // rpm_limit 始终序列化（即便等于默认 10），保证 0=不限速 不被吞
        assert!(json.contains("rpmLimit"));
    }

    #[test]
    fn test_default_credentials_path() {
        assert_eq!(
            KiroCredentials::default_credentials_path(),
            "credentials.json"
        );
    }

    #[test]
    fn test_external_idp_fields_roundtrip() {
        // external_idp 凭据：tokenEndpoint / issuerUrl / scopes 往返序列化
        let json = r#"{
            "accessToken": "azure-access",
            "refreshToken": "azure-refresh-token-that-is-long-enough-to-pass",
            "authMethod": "external_idp",
            "provider": "AzureAD",
            "clientId": "11111111-2222-3333-4444-555555555555",
            "tokenEndpoint": "https://login.microsoftonline.com/tenant/oauth2/v2.0/token",
            "issuerUrl": "https://login.microsoftonline.com/tenant/v2.0",
            "scopes": "openid profile offline_access"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert!(creds.is_external_idp());
        // 与 API Key 互斥
        assert!(!creds.is_api_key_credential());
        assert_eq!(
            creds.token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant/oauth2/v2.0/token")
        );
        assert_eq!(
            creds.scopes.as_deref(),
            Some("openid profile offline_access")
        );

        // 重新序列化后字段仍在
        let out = creds.to_pretty_json().unwrap();
        assert!(out.contains("tokenEndpoint"));
        assert!(out.contains("issuerUrl"));
        assert!(out.contains("scopes"));
    }

    #[test]
    fn test_is_external_idp_case_insensitive_and_false_default() {
        let mut creds = KiroCredentials::default();
        assert!(!creds.is_external_idp());
        creds.auth_method = Some("External_IDP".to_string());
        assert!(creds.is_external_idp());
        creds.auth_method = Some("social".to_string());
        assert!(!creds.is_external_idp());
    }

    #[test]
    fn test_is_placeholder_profile_arn() {
        assert!(is_placeholder_profile_arn(BUILDER_ID_PROFILE_ARN));
        assert!(!is_placeholder_profile_arn(SOCIAL_PROFILE_ARN));
        assert!(!is_placeholder_profile_arn(
            "arn:aws:codewhisperer:us-east-1:123456789012:profile/REAL123"
        ));
    }

    #[test]
    fn test_effective_profile_arn_skips_placeholder() {
        // BuilderID 占位符 → None（不发送给上游）
        let mut cred = KiroCredentials::default();
        cred.profile_arn = Some(BUILDER_ID_PROFILE_ARN.to_string());
        assert_eq!(cred.effective_profile_arn(), None);

        // Social 共享 ARN → 原样返回
        cred.profile_arn = Some(SOCIAL_PROFILE_ARN.to_string());
        assert_eq!(cred.effective_profile_arn(), Some(SOCIAL_PROFILE_ARN));

        // 真实 Enterprise ARN → 原样返回
        let real = "arn:aws:codewhisperer:us-east-1:123456789012:profile/REAL123";
        cred.profile_arn = Some(real.to_string());
        assert_eq!(cred.effective_profile_arn(), Some(real));

        // 无 ARN → None
        cred.profile_arn = None;
        assert_eq!(cred.effective_profile_arn(), None);
    }

    #[test]
    fn test_streaming_profile_arn_includes_placeholder() {
        // 流式端点：显式 BuilderID 占位符原样发送，缺失会被上游以 400 拒绝
        let mut cred = KiroCredentials::default();
        cred.profile_arn = Some(BUILDER_ID_PROFILE_ARN.to_string());
        assert_eq!(
            cred.streaming_profile_arn().as_deref(),
            Some(BUILDER_ID_PROFILE_ARN)
        );

        // 真实 ARN 原样发送
        let real = "arn:aws:codewhisperer:us-east-1:123456789012:profile/REAL123";
        cred.profile_arn = Some(real.to_string());
        assert_eq!(cred.streaming_profile_arn().as_deref(), Some(real));

        // 未填充 + 非 social（BuilderID 账号）→ 回退 BuilderID 占位符
        let mut builder = KiroCredentials::default();
        builder.profile_arn = None;
        builder.refresh_token = Some("r".to_string());
        assert_eq!(
            builder.streaming_profile_arn().as_deref(),
            Some(BUILDER_ID_PROFILE_ARN)
        );

        // 未填充 + social → 回退 Social 共享 ARN（非占位符，原样发送）
        let mut social = KiroCredentials::default();
        social.profile_arn = None;
        social.auth_method = Some("social".to_string());
        assert_eq!(
            social.streaming_profile_arn().as_deref(),
            Some(SOCIAL_PROFILE_ARN)
        );

        // API Key 凭据无 profileArn 概念 → None
        let mut api = KiroCredentials::default();
        api.kiro_api_key = Some("ksk_xxx".to_string());
        assert_eq!(api.streaming_profile_arn(), None);
    }

    #[test]
    fn test_priority_default() {
        let json = r#"{"refreshToken": "test"}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 0);
    }

    #[test]
    fn test_priority_explicit() {
        let json = r#"{"refreshToken": "test", "priority": 5}"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.priority, 5);
    }

    #[test]
    fn test_credentials_config_single() {
        let json = r#"{"refreshToken": "test", "expiresAt": "2025-12-31T00:00:00Z"}"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Single(_)));
    }

    #[test]
    fn test_credentials_config_multiple() {
        let json = r#"[
            {"refreshToken": "test1", "priority": 1},
            {"refreshToken": "test2", "priority": 0}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        assert!(matches!(config, CredentialsConfig::Multiple(_)));
        assert_eq!(config.into_sorted_credentials().len(), 2);
    }

    #[test]
    fn test_credentials_config_priority_sorting() {
        let json = r#"[
            {"refreshToken": "t1", "priority": 2},
            {"refreshToken": "t2", "priority": 0},
            {"refreshToken": "t3", "priority": 1}
        ]"#;
        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        // 验证按优先级排序
        assert_eq!(list[0].refresh_token, Some("t2".to_string())); // priority 0
        assert_eq!(list[1].refresh_token, Some("t3".to_string())); // priority 1
        assert_eq!(list[2].refresh_token, Some("t1".to_string())); // priority 2
    }

    // ============ Region 字段测试 ============

    #[test]
    fn test_region_field_parsing() {
        // 测试解析包含 region 字段的 JSON
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, Some("us-east-1".to_string()));
    }

    #[test]
    fn test_region_field_missing_backward_compat() {
        // 测试向后兼容：不包含 region 字段的旧格式 JSON
        let json = r#"{
            "refreshToken": "test_refresh",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.region, None);
    }

    #[test]
    fn test_region_field_serialization() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            provider: None,
            client_id: None,
            client_secret: None,
            start_url: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: 0,
            rpm_limit: 10,
            region: Some("eu-west-1".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            disabled_reason: None,
            self_heal_consecutive_rounds: 0,
            self_heal_total_count: 0,
            last_self_heal_at: None,
            self_heal_model: None,
            kiro_api_key: None,
            endpoint: None,
            groups: vec![],
            source_channel: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("region"));
        assert!(json.contains("eu-west-1"));
    }

    #[test]
    fn test_region_field_none_not_serialized() {
        let creds = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: Some("test".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: None,
            provider: None,
            client_id: None,
            client_secret: None,
            start_url: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: 0,
            rpm_limit: 10,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            disabled_reason: None,
            self_heal_consecutive_rounds: 0,
            self_heal_total_count: 0,
            last_self_heal_at: None,
            self_heal_model: None,
            kiro_api_key: None,
            endpoint: None,
            groups: vec![],
            source_channel: None,
        };

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("region"));
    }

    // ============ MachineId 字段测试 ============

    #[test]
    fn test_machine_id_field_parsing() {
        let machine_id = "a".repeat(64);
        let json = format!(
            r#"{{
                "refreshToken": "test_refresh",
                "machineId": "{machine_id}"
            }}"#
        );

        let creds = KiroCredentials::from_json(&json).unwrap();
        assert_eq!(creds.refresh_token, Some("test_refresh".to_string()));
        assert_eq!(creds.machine_id, Some(machine_id));
    }

    #[test]
    fn test_machine_id_field_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = Some("b".repeat(64));

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("machineId"));
    }

    #[test]
    fn test_machine_id_field_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.machine_id = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("machineId"));
    }

    #[test]
    fn test_multiple_credentials_with_different_regions() {
        // 测试多凭据场景下不同凭据使用各自的 region
        let json = r#"[
            {"refreshToken": "t1", "region": "us-east-1"},
            {"refreshToken": "t2", "region": "eu-west-1"},
            {"refreshToken": "t3"}
        ]"#;

        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let list = config.into_sorted_credentials();

        assert_eq!(list[0].region, Some("us-east-1".to_string()));
        assert_eq!(list[1].region, Some("eu-west-1".to_string()));
        assert_eq!(list[2].region, None);
    }

    #[test]
    fn test_region_field_with_all_fields() {
        // 测试包含所有字段的完整 JSON
        let json = r#"{
            "id": 1,
            "accessToken": "access",
            "refreshToken": "refresh",
            "profileArn": "arn:aws:test",
            "expiresAt": "2025-12-31T00:00:00Z",
            "authMethod": "idc",
            "clientId": "client123",
            "clientSecret": "secret456",
            "priority": 5,
            "region": "ap-northeast-1"
        }"#;

        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.id, Some(1));
        assert_eq!(creds.access_token, Some("access".to_string()));
        assert_eq!(creds.refresh_token, Some("refresh".to_string()));
        assert_eq!(creds.profile_arn, Some("arn:aws:test".to_string()));
        assert_eq!(creds.expires_at, Some("2025-12-31T00:00:00Z".to_string()));
        assert_eq!(creds.auth_method, Some("idc".to_string()));
        assert_eq!(creds.client_id, Some("client123".to_string()));
        assert_eq!(creds.client_secret, Some("secret456".to_string()));
        assert_eq!(creds.priority, 5);
        assert_eq!(creds.region, Some("ap-northeast-1".to_string()));
    }

    #[test]
    fn test_region_roundtrip() {
        // 测试序列化和反序列化的往返一致性
        let original = KiroCredentials {
            id: Some(42),
            access_token: Some("token".to_string()),
            refresh_token: Some("refresh".to_string()),
            profile_arn: None,
            expires_at: None,
            auth_method: Some("social".to_string()),
            provider: None,
            client_id: None,
            client_secret: None,
            start_url: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: 3,
            rpm_limit: 10,
            region: Some("us-west-2".to_string()),
            auth_region: None,
            api_region: None,
            machine_id: Some("c".repeat(64)),
            email: None,
            subscription_title: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            disabled: false,
            disabled_reason: None,
            self_heal_consecutive_rounds: 0,
            self_heal_total_count: 0,
            last_self_heal_at: None,
            self_heal_model: None,
            kiro_api_key: None,
            endpoint: None,
            groups: vec![],
            source_channel: None,
        };

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.access_token, original.access_token);
        assert_eq!(parsed.refresh_token, original.refresh_token);
        assert_eq!(parsed.priority, original.priority);
        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.machine_id, original.machine_id);
    }

    // ============ auth_region / api_region 字段测试 ============

    #[test]
    fn test_auth_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "authRegion": "eu-central-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.auth_region, Some("eu-central-1".to_string()));
        assert_eq!(creds.api_region, None);
    }

    #[test]
    fn test_api_region_field_parsing() {
        let json = r#"{
            "refreshToken": "test_refresh",
            "apiRegion": "ap-southeast-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.api_region, Some("ap-southeast-1".to_string()));
        assert_eq!(creds.auth_region, None);
    }

    #[test]
    fn test_auth_api_region_serialization() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = Some("eu-west-1".to_string());
        creds.api_region = Some("us-west-2".to_string());

        let json = creds.to_pretty_json().unwrap();
        assert!(json.contains("authRegion"));
        assert!(json.contains("eu-west-1"));
        assert!(json.contains("apiRegion"));
        assert!(json.contains("us-west-2"));
    }

    #[test]
    fn test_auth_api_region_none_not_serialized() {
        let mut creds = KiroCredentials::default();
        creds.refresh_token = Some("test".to_string());
        creds.auth_region = None;
        creds.api_region = None;

        let json = creds.to_pretty_json().unwrap();
        assert!(!json.contains("authRegion"));
        assert!(!json.contains("apiRegion"));
    }

    #[test]
    fn test_auth_api_region_roundtrip() {
        let mut original = KiroCredentials::default();
        original.refresh_token = Some("refresh".to_string());
        original.region = Some("us-east-1".to_string());
        original.auth_region = Some("eu-west-1".to_string());
        original.api_region = Some("ap-northeast-1".to_string());

        let json = original.to_pretty_json().unwrap();
        let parsed = KiroCredentials::from_json(&json).unwrap();

        assert_eq!(parsed.region, original.region);
        assert_eq!(parsed.auth_region, original.auth_region);
        assert_eq!(parsed.api_region, original.api_region);
    }

    #[test]
    fn test_backward_compat_no_auth_api_region() {
        // 旧格式 JSON 不包含 authRegion/apiRegion，应正常解析
        let json = r#"{
            "refreshToken": "test_refresh",
            "region": "us-east-1"
        }"#;
        let creds = KiroCredentials::from_json(json).unwrap();
        assert_eq!(creds.region, Some("us-east-1".to_string()));
        assert_eq!(creds.auth_region, None);
        assert_eq!(creds.api_region, None);
    }

    // ============ effective_auth_region / effective_api_region 优先级测试 ============

    #[test]
    fn test_effective_auth_region_credential_auth_region_highest() {
        // 凭据.auth_region > 凭据.region > config.auth_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());
        creds.auth_region = Some("cred-auth-region".to_string());

        assert_eq!(creds.effective_auth_region(&config), "cred-auth-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_credential_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());
        // auth_region 未设置

        assert_eq!(creds.effective_auth_region(&config), "cred-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_auth_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.auth_region = Some("config-auth-region".to_string());

        let creds = KiroCredentials::default();
        // auth_region 和 region 均未设置

        assert_eq!(creds.effective_auth_region(&config), "config-auth-region");
    }

    #[test]
    fn test_effective_auth_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        // config.auth_region 未设置

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_auth_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_credential_api_region_highest() {
        // 凭据.api_region > config.api_region > config.region
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let mut creds = KiroCredentials::default();
        creds.api_region = Some("cred-api-region".to_string());

        assert_eq!(creds.effective_api_region(&config), "cred-api-region");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_api_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();
        config.api_region = Some("config-api-region".to_string());

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-api-region");
    }

    #[test]
    fn test_effective_api_region_fallback_to_config_region() {
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let creds = KiroCredentials::default();

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_effective_api_region_ignores_credential_region() {
        // 凭据.region 不参与 api_region 的回退链
        let mut config = Config::default();
        config.region = "config-region".to_string();

        let mut creds = KiroCredentials::default();
        creds.region = Some("cred-region".to_string());

        assert_eq!(creds.effective_api_region(&config), "config-region");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let mut creds = KiroCredentials::default();
        creds.auth_region = Some("auth-only".to_string());
        creds.api_region = Some("api-only".to_string());

        assert_eq!(creds.effective_auth_region(&config), "auth-only");
        assert_eq!(creds.effective_api_region(&config), "api-only");
    }

    // ============ 凭据级代理优先级测试 ============

    #[test]
    fn test_effective_proxy_candidates_credential_overrides_global() {
        let global = vec![Some(ProxyConfig::new("http://global:8080"))];
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("socks5://cred:1080".to_string());

        let result = creds.effective_proxy_candidates(&global);
        assert_eq!(result, vec![Some(ProxyConfig::new("socks5://cred:1080"))]);
    }

    #[test]
    fn test_effective_proxy_candidates_credential_with_auth() {
        let global = vec![Some(ProxyConfig::new("http://global:8080"))];
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("http://proxy:3128".to_string());
        creds.proxy_username = Some("user".to_string());
        creds.proxy_password = Some("pass".to_string());

        let result = creds.effective_proxy_candidates(&global);
        let expected = ProxyConfig::new("http://proxy:3128").with_auth("user", "pass");
        assert_eq!(result, vec![Some(expected)]);
    }

    #[test]
    fn test_effective_proxy_candidates_direct_bypasses_global() {
        let global = vec![Some(ProxyConfig::new("http://global:8080"))];
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("direct".to_string());

        let result = creds.effective_proxy_candidates(&global);
        assert_eq!(result, vec![None]);
    }

    #[test]
    fn test_effective_proxy_candidates_direct_case_insensitive() {
        let global = vec![Some(ProxyConfig::new("http://global:8080"))];
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("DIRECT".to_string());

        let result = creds.effective_proxy_candidates(&global);
        assert_eq!(result, vec![None]);
    }

    #[test]
    fn test_effective_proxy_candidates_fallback_to_global() {
        let global = vec![Some(ProxyConfig::new("http://global:8080"))];
        let creds = KiroCredentials::default();

        let result = creds.effective_proxy_candidates(&global);
        assert_eq!(result, global);
    }

    #[test]
    fn test_effective_proxy_candidates_parse_multiple_and_direct() {
        let mut creds = KiroCredentials::default();
        creds.proxy_url = Some("socks5://p1:1080, direct http://p2:8080".to_string());
        let result = creds.effective_proxy_candidates(&[]);
        assert_eq!(
            result,
            vec![
                Some(ProxyConfig::new("socks5://p1:1080")),
                None,
                Some(ProxyConfig::new("http://p2:8080")),
            ]
        );
    }

    #[test]
    fn test_effective_proxy_candidates_fallback_to_global_candidates() {
        let creds = KiroCredentials::default();
        let global = vec![Some(ProxyConfig::new("http://global:8080")), None];
        assert_eq!(creds.effective_proxy_candidates(&global), global);
    }

    #[test]
    fn test_effective_proxy_candidates_none_when_no_proxy() {
        let creds = KiroCredentials::default();
        let result = creds.effective_proxy_candidates(&[]);
        assert_eq!(result, vec![None]);
    }

    // ============ 企业 SSO (external_idp) 测试 ============

    #[test]
    fn test_canonicalize_external_idp_aliases() {
        for alias in [
            "external_idp",
            "AzureAD",
            "azure",
            "Entra",
            "entra-id",
            "microsoft",
            "M365",
            "office365",
            "external",
        ] {
            assert_eq!(
                canonicalize_auth_method_value(alias),
                "external_idp",
                "别名 {:?} 应规范化为 external_idp",
                alias
            );
        }
        // 不误伤其它方式
        assert_eq!(canonicalize_auth_method_value("social"), "social");
        assert_eq!(canonicalize_auth_method_value("builder-id"), "idc");
        assert_eq!(canonicalize_auth_method_value("apikey"), "api_key");
    }

    #[test]
    fn test_normalize_import_auth_method_inference() {
        // 显式别名 → external_idp
        assert_eq!(
            normalize_import_auth_method("azuread", None),
            "external_idp"
        );
        assert_eq!(
            normalize_import_auth_method_from_fields(ExternalIdpImportFields {
                auth_method: Some("social"),
                provider: Some("AzureAD"),
                idp: None,
                token_endpoint: None,
                issuer_url: None,
                scopes: None,
                user_id: Some("https://login.microsoftonline.com/tenant/v2.0.object-id"),
                access_token: None,
                client_id: Some("client-id"),
            }),
            "external_idp"
        );
        // 带 tokenEndpoint 但未声明（默认 social）→ 推断 external_idp
        assert_eq!(
            normalize_import_auth_method(
                "social",
                Some("https://login.microsoftonline.com/t/oauth2/v2.0/token")
            ),
            "external_idp"
        );
        // 空 tokenEndpoint 不触发推断
        assert_eq!(
            normalize_import_auth_method("social", Some("   ")),
            "social"
        );
        assert_eq!(normalize_import_auth_method("social", None), "social");
        // idc 保持
        assert_eq!(normalize_import_auth_method("idc", None), "idc");
    }

    fn unsigned_jwt(payload_json: &str) -> String {
        let header = general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        format!("{header}.{payload}.")
    }

    #[test]
    fn test_complete_external_idp_import_fields_from_old_kam_user_id() {
        let completed = complete_external_idp_import_fields(ExternalIdpImportFields {
            auth_method: Some("external_idp"),
            provider: Some("AzureAD"),
            idp: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            user_id: Some("https://login.microsoftonline.com/tenant-123/v2.0.object-id"),
            access_token: None,
            client_id: Some("client-123"),
        });

        assert_eq!(
            completed.token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant-123/oauth2/v2.0/token")
        );
        assert_eq!(
            completed.issuer_url.as_deref(),
            Some("https://login.microsoftonline.com/tenant-123/v2.0")
        );
        assert_eq!(
            completed.scopes.as_deref(),
            Some(
                "api://client-123/codewhisperer:conversations api://client-123/codewhisperer:completions offline_access"
            )
        );
    }

    #[test]
    fn test_complete_external_idp_import_fields_from_jwt_claims() {
        let access_token = unsigned_jwt(
            r#"{"iss":"https://login.microsoftonline.com/tenant-abc/v2.0","scp":"codewhisperer:conversations codewhisperer:completions"}"#,
        );
        let completed = complete_external_idp_import_fields(ExternalIdpImportFields {
            auth_method: Some("external_idp"),
            provider: Some("AzureAD"),
            idp: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            user_id: None,
            access_token: Some(&access_token),
            client_id: Some("client-abc"),
        });

        assert_eq!(
            completed.token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/tenant-abc/oauth2/v2.0/token")
        );
        assert_eq!(
            completed.scopes.as_deref(),
            Some(
                "api://client-abc/codewhisperer:conversations api://client-abc/codewhisperer:completions offline_access"
            )
        );
    }

    #[test]
    fn test_normalize_external_idp_scopes_keeps_full_api_scopes() {
        let scopes = normalize_external_idp_scopes(
            "api://client-id/codewhisperer:conversations offline_access codewhisperer:completions",
            Some("client-id"),
        );

        assert_eq!(
            scopes.as_deref(),
            Some(
                "api://client-id/codewhisperer:conversations offline_access api://client-id/codewhisperer:completions"
            )
        );
    }

    #[test]
    fn test_validate_external_idp_endpoint() {
        // 合法 Microsoft 主机
        assert!(
            validate_external_idp_endpoint(
                "https://login.microsoftonline.com/tenant/oauth2/v2.0/token"
            )
            .is_ok()
        );
        assert!(validate_external_idp_endpoint("https://login.microsoftonline.us/t/token").is_ok());
        // 非 https 拒绝
        assert!(validate_external_idp_endpoint("http://login.microsoftonline.com/t").is_err());
        // IP 字面量拒绝
        assert!(validate_external_idp_endpoint("https://127.0.0.1/token").is_err());
        assert!(validate_external_idp_endpoint("https://[::1]/token").is_err());
        // 允许列表外拒绝
        assert!(validate_external_idp_endpoint("https://evil.example.com/token").is_err());
        // 前导点锚定：evil-microsoftonline.com 不应命中
        assert!(validate_external_idp_endpoint("https://evil-microsoftonline.com/token").is_err());
        // 裸域（无子域）不应命中 .microsoftonline.com 后缀
        assert!(validate_external_idp_endpoint("https://microsoftonline.com/token").is_err());
    }

    #[test]
    fn test_is_external_idp_and_token_type_header() {
        let mut cred = KiroCredentials {
            auth_method: Some("azuread".to_string()), // 别名也应识别
            ..Default::default()
        };
        assert!(cred.is_external_idp_credential());
        assert_eq!(cred.token_type_header(), Some("EXTERNAL_IDP"));

        cred.auth_method = Some("social".to_string());
        assert!(!cred.is_external_idp_credential());
        assert_eq!(cred.token_type_header(), None);

        cred.auth_method = Some("api_key".to_string());
        assert_eq!(cred.token_type_header(), Some("API_KEY"));
    }

    #[test]
    fn test_external_idp_credentials_serde_roundtrip() {
        let json = r#"{
            "authMethod": "external_idp",
            "refreshToken": "rt",
            "clientId": "fa6d79bf-xxxx",
            "tokenEndpoint": "https://login.microsoftonline.com/t/oauth2/v2.0/token",
            "issuerUrl": "https://login.microsoftonline.com/t/v2.0",
            "scopes": "openid profile offline_access",
            "region": "eu-central-1"
        }"#;
        let cred = KiroCredentials::from_json(json).unwrap();
        assert_eq!(cred.auth_method.as_deref(), Some("external_idp"));
        assert_eq!(
            cred.token_endpoint.as_deref(),
            Some("https://login.microsoftonline.com/t/oauth2/v2.0/token")
        );
        assert_eq!(
            cred.scopes.as_deref(),
            Some("openid profile offline_access")
        );

        // 序列化后应保留新字段（camelCase）
        let serialized = cred.to_pretty_json().unwrap();
        assert!(serialized.contains("\"tokenEndpoint\""));
        assert!(serialized.contains("\"issuerUrl\""));
        assert!(serialized.contains("\"scopes\""));

        // 空字段不应出现在序列化结果中
        let empty = KiroCredentials::default();
        let empty_json = empty.to_pretty_json().unwrap();
        assert!(!empty_json.contains("tokenEndpoint"));
    }
}
