use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// 工具兼容模式。
///
/// - `ClaudeCode`（默认）：把 Claude Code 内置工具（Write/Edit/Bash/Read/Glob/Grep/LS/WebSearch）
///   的工具名与入参双向适配为 Kiro 内置工具（fs_write/str_replace/... ），并替换为 Kiro 内置 schema。
/// - `Raw`：保留旧行为，直接透传客户端工具名/schema，用于排障。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCompatibilityMode {
    #[default]
    ClaudeCode,
    Raw,
}

/// 普通 429 的重试策略模式。
///
/// `Failover` 是本项目默认策略：普通 429 先用同一凭据切换 q/runtime 独立限流桶，
/// 备用端点仍失败时再在本次请求内换凭据，且不给凭据施加跨请求冷却。其它模式来自
/// Kiro-RS-Tool，用于按需切换为更激进或更保守的普通 429 冷却与重试节奏。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RetryMode {
    #[default]
    Failover,
    Turbo,
    Fast,
    Balanced,
    Steady,
    Polite,
    Custom,
}

impl std::fmt::Display for RetryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Failover => "failover",
            Self::Turbo => "turbo",
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Steady => "steady",
            Self::Polite => "polite",
            Self::Custom => "custom",
        };
        f.write_str(value)
    }
}

impl std::str::FromStr for RetryMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "failover" | "current" | "default" => Ok(Self::Failover),
            "turbo" => Ok(Self::Turbo),
            "fast" => Ok(Self::Fast),
            "balanced" => Ok(Self::Balanced),
            "steady" => Ok(Self::Steady),
            "polite" => Ok(Self::Polite),
            "custom" => Ok(Self::Custom),
            _ => anyhow::bail!("无效的重试模式: {}", value),
        }
    }
}

/// 普通 429 的可配置重试策略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    /// 普通 429 后的跨请求冷却时间；0 表示不进入跨请求冷却。
    pub rate_limit_cooldown_ms: u64,
    /// 每个凭据的请求重试预算。非默认策略会按账号数放大，并受全局上限保护。
    pub max_request_retries: usize,
    /// 指数退避基础时长。
    pub base_backoff_ms: u64,
    /// 指数退避最大时长。
    pub max_backoff_ms: u64,
    /// 普通 429 后是否优先切换其它凭据。
    pub credential_switch_on_429: bool,
    /// 是否尊重上游 Retry-After 头。
    pub respect_retry_after: bool,
}

impl RetryPolicy {
    pub fn preset(mode: RetryMode) -> Self {
        match mode {
            RetryMode::Failover => Self {
                rate_limit_cooldown_ms: 0,
                max_request_retries: 3,
                base_backoff_ms: 1_000,
                max_backoff_ms: 8_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Turbo => Self {
                rate_limit_cooldown_ms: 1_000,
                max_request_retries: 12,
                base_backoff_ms: 100,
                max_backoff_ms: 1_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Fast => Self {
                rate_limit_cooldown_ms: 3_000,
                max_request_retries: 9,
                base_backoff_ms: 200,
                max_backoff_ms: 2_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Balanced => Self {
                rate_limit_cooldown_ms: 10_000,
                max_request_retries: 9,
                base_backoff_ms: 500,
                max_backoff_ms: 5_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Steady => Self {
                rate_limit_cooldown_ms: 30_000,
                max_request_retries: 6,
                base_backoff_ms: 1_000,
                max_backoff_ms: 10_000,
                credential_switch_on_429: true,
                respect_retry_after: true,
            },
            RetryMode::Polite => Self {
                rate_limit_cooldown_ms: 60_000,
                max_request_retries: 4,
                base_backoff_ms: 2_000,
                max_backoff_ms: 30_000,
                credential_switch_on_429: false,
                respect_retry_after: true,
            },
            RetryMode::Custom => Self::preset(RetryMode::Fast),
        }
    }

    pub fn effective(mode: RetryMode, custom: Option<&RetryPolicy>) -> anyhow::Result<Self> {
        let policy = if mode == RetryMode::Custom {
            custom
                .cloned()
                .unwrap_or_else(|| Self::preset(RetryMode::Fast))
        } else {
            Self::preset(mode)
        };

        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.rate_limit_cooldown_ms > 120_000 {
            anyhow::bail!("rateLimitCooldownMs 必须在 0..=120000 之间");
        }
        if !(1..=30).contains(&self.max_request_retries) {
            anyhow::bail!("maxRequestRetries 必须在 1..=30 之间");
        }
        if !(50..=30_000).contains(&self.base_backoff_ms) {
            anyhow::bail!("baseBackoffMs 必须在 50..=30000 之间");
        }
        if self.max_backoff_ms < self.base_backoff_ms || self.max_backoff_ms > 120_000 {
            anyhow::bail!("maxBackoffMs 必须在 baseBackoffMs..=120000 之间");
        }
        Ok(())
    }
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 负载均衡模式（"priority" / "balanced" / "least_conn"，默认 "least_conn" 最少负载）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 代理均衡模式（"sticky" / "round_robin" / "least_load"）
    #[serde(default = "default_proxy_balancing_mode")]
    pub proxy_balancing_mode: String,

    /// 账号级 429 风控触发时是否对当前凭据进入冷却并故障转移（默认 true）。
    ///
    /// 关闭后：429 + suspicious activity 仍按普通瞬态错误重试，不切换凭据。
    /// 开启后：识别到 suspicious activity 字符串时，把当前凭据冷却 `account_throttle_cooldown_secs` 秒，
    /// 立即切换到下一个可用凭据。
    #[serde(default = "default_account_throttle_failover")]
    pub account_throttle_failover: bool,

    /// 账号级风控冷却时长（秒，默认 1800 = 30 分钟）。
    #[serde(default = "default_account_throttle_cooldown_secs")]
    pub account_throttle_cooldown_secs: u64,

    /// 是否识别 403 账号封禁文案并立即禁用凭据（默认 true）。
    ///
    /// 开启后：某凭据收到 403 且响应体命中明确封禁文案（同时含 "suspended" 与
    /// "locked your account"）时，立即标记为 `Suspended` 并禁用。这类凭据**不参与
    /// 自愈**，需人工联系客服核实后手动重置，从根上打断持续 403 死循环（issue #51）。
    ///
    /// 只匹配这两个高特异短语同时出现的情形，不影响普通 403（权限/WAF/区域抖动），
    /// 后者仍按既有 `report_failure` 累计路径处理。关闭后：完全回退旧行为。
    #[serde(default = "default_suspended_detection_enabled")]
    pub suspended_detection_enabled: bool,

    /// 是否启用凭据自愈（默认 true）。
    ///
    /// 当前请求的 model/group 作用域没有可用凭据时，只恢复该作用域内因
    /// `TooManyFailures` 被自动禁用且仍满足冷却/上限的凭据。
    #[serde(default = "default_self_heal_enabled")]
    pub self_heal_enabled: bool,

    /// 同一凭据两次自愈之间的最小冷却间隔（秒，默认 300 = 5 分钟）。
    ///
    /// 冷却窗口内即使再次全灭也不触发自愈。这是打断 issue #51「全禁 → 自愈 →
    /// 403 → 再禁」死循环的关键：持续故障时自愈频率被限到每 5 分钟一次，
    /// 而非每个请求都重置刷屏并无效打上游。
    #[serde(default = "default_self_heal_min_interval_secs")]
    pub self_heal_min_interval_secs: u64,

    /// 连续自愈的最大轮数（默认 5，`0` 表示不限）。
    ///
    /// 现状口径（单凭据单槽）：每条凭据只维护一个自愈 streak，并打上触发时的模型标签。
    /// 同模型成功才会清零；换模型请求不会重置该计数，也不会换模型继续自愈。
    /// `failure_count` 仍按凭据全局累计；达上限后保持禁用，需人工在 Admin 启用。
    #[serde(default = "default_self_heal_max_consecutive_rounds")]
    pub self_heal_max_consecutive_rounds: u32,

    /// 普通 429 重试策略模式。默认 `failover` 保持当前项目行为。
    #[serde(default = "default_retry_mode")]
    pub retry_mode: RetryMode,

    /// `retry_mode = custom` 时使用的普通 429 自定义策略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_policy: Option<RetryPolicy>,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 工具兼容模式。默认 `claude-code`：把 Claude Code 内置工具名/入参双向适配为
    /// Kiro 内置工具；`raw` 保留旧行为、直接透传客户端工具 schema，用于排障。
    #[serde(default = "default_tool_compatibility_mode")]
    pub tool_compatibility_mode: ToolCompatibilityMode,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 是否启用请求链路追踪（写 traces.db）。默认 true。
    ///
    /// 关闭后：不再写入 trace 记录、不走 TraceSink，但 `GET /api/admin/traces`
    /// 仍可查询历史已存记录。适合隐私敏感或磁盘紧张的场景。
    #[serde(default = "default_trace_enabled")]
    pub trace_enabled: bool,

    /// 请求链路追踪记录保留天数（默认 7）。后台任务每天清理超期记录。
    #[serde(default = "default_trace_retention_days")]
    pub trace_retention_days: u32,

    /// 请求用量日志（usage_log.*.jsonl + 聚合桶）保留天数（默认 31）。
    #[serde(default = "default_usage_log_retention_days")]
    pub usage_log_retention_days: u32,

    /// 每凭据模型列表缓存的 TTL（秒，默认 3600）。过期后下一次使用时刷新。
    #[serde(default = "default_model_cache_ttl_secs")]
    pub model_cache_ttl_secs: u64,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "2.3.0".to_string()
}

fn default_system_version() -> String {
    "macos".to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    // 默认最少负载：把请求优先分给当前在途请求数最少的凭据，避免高优先级凭据被打爆。
    "least_conn".to_string()
}

fn default_proxy_balancing_mode() -> String {
    "sticky".to_string()
}

fn default_model_cache_ttl_secs() -> u64 {
    3600
}

fn default_account_throttle_failover() -> bool {
    true
}

fn default_account_throttle_cooldown_secs() -> u64 {
    30 * 60
}

fn default_suspended_detection_enabled() -> bool {
    true
}

fn default_self_heal_enabled() -> bool {
    true
}

fn default_self_heal_min_interval_secs() -> u64 {
    5 * 60
}

fn default_self_heal_max_consecutive_rounds() -> u32 {
    5
}

fn default_retry_mode() -> RetryMode {
    RetryMode::Failover
}

fn default_extract_thinking() -> bool {
    true
}

fn default_tool_compatibility_mode() -> ToolCompatibilityMode {
    ToolCompatibilityMode::ClaudeCode
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

fn default_trace_enabled() -> bool {
    true
}

fn default_trace_retention_days() -> u32 {
    7
}

fn default_usage_log_retention_days() -> u32 {
    31
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            load_balancing_mode: default_load_balancing_mode(),
            proxy_balancing_mode: default_proxy_balancing_mode(),
            account_throttle_failover: default_account_throttle_failover(),
            account_throttle_cooldown_secs: default_account_throttle_cooldown_secs(),
            suspended_detection_enabled: default_suspended_detection_enabled(),
            self_heal_enabled: default_self_heal_enabled(),
            self_heal_min_interval_secs: default_self_heal_min_interval_secs(),
            self_heal_max_consecutive_rounds: default_self_heal_max_consecutive_rounds(),
            retry_mode: default_retry_mode(),
            retry_policy: None,
            extract_thinking: default_extract_thinking(),
            tool_compatibility_mode: default_tool_compatibility_mode(),
            default_endpoint: default_endpoint(),
            trace_enabled: default_trace_enabled(),
            trace_retention_days: default_trace_retention_days(),
            usage_log_retention_days: default_usage_log_retention_days(),
            model_cache_ttl_secs: default_model_cache_ttl_secs(),
            endpoints: HashMap::new(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());

        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件（同目录临时文件 + rename 原子替换）
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        let tmp = path.with_extension("json.tmp");
        let write_result = fs::write(&tmp, &content).and_then(|()| fs::rename(&tmp, path));
        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("写入配置文件失败: {}", path.display()));
        }
        Ok(())
    }

    /// 进程内共享写锁下的「重新加载 → 修改 → 原子保存」。
    ///
    /// 所有运行时配置写盘统一走这里，避免并发 setter 之间读改写互相覆盖。
    pub fn update_file<P: AsRef<Path>>(
        path: P,
        updater: impl FnOnce(&mut Config),
    ) -> anyhow::Result<()> {
        let path = path.as_ref();
        let _guard = config_file_write_lock().lock();
        let mut config =
            Self::load(path).with_context(|| format!("重新加载配置失败: {}", path.display()))?;
        updater(&mut config);
        config
            .save()
            .with_context(|| format!("持久化配置失败: {}", path.display()))?;
        Ok(())
    }
}

/// 全进程共享的 config 文件写锁（覆盖 load-modify-save 整个临界区）。
fn config_file_write_lock() -> &'static parking_lot::Mutex<()> {
    static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    &LOCK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_heal_config_defaults_for_existing_configs() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(config.suspended_detection_enabled);
        assert!(config.self_heal_enabled);
        assert_eq!(config.self_heal_min_interval_secs, 300);
        assert_eq!(config.self_heal_max_consecutive_rounds, 5);

        let default = Config::default();
        assert!(default.suspended_detection_enabled);
        assert!(default.self_heal_enabled);
        assert_eq!(default.self_heal_min_interval_secs, 300);
        assert_eq!(default.self_heal_max_consecutive_rounds, 5);
    }

    #[test]
    fn self_heal_config_accepts_explicit_values() {
        let config: Config = serde_json::from_str(
            r#"{
                "suspendedDetectionEnabled": false,
                "selfHealEnabled": false,
                "selfHealMinIntervalSecs": 60,
                "selfHealMaxConsecutiveRounds": 0
            }"#,
        )
        .unwrap();
        assert!(!config.suspended_detection_enabled);
        assert!(!config.self_heal_enabled);
        assert_eq!(config.self_heal_min_interval_secs, 60);
        assert_eq!(config.self_heal_max_consecutive_rounds, 0);
    }

    #[test]
    fn concurrent_update_file_preserves_all_fields() {
        let dir = std::env::temp_dir().join(format!(
            "kiro-config-lock-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        Config::load(&path).unwrap().save().unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for (interval, rounds) in [(123_u64, 7_u32), (456, 9)] {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                Config::update_file(&path, move |config| {
                    config.self_heal_min_interval_secs = interval;
                    config.self_heal_max_consecutive_rounds = rounds;
                })
                .unwrap();
            }));
        }
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        // 写锁串行化整个 load-modify-save：无锁时两线程会争抢同一 config.json.tmp，
        // rename 后源文件消失导致另一线程 unwrap panic。有锁时最终文件必须是某一次
        // 完整写入（两字段同源），且不得残留临时文件。
        let persisted = Config::load(&path).unwrap();
        assert!(
            (persisted.self_heal_min_interval_secs == 123
                && persisted.self_heal_max_consecutive_rounds == 7)
                || (persisted.self_heal_min_interval_secs == 456
                    && persisted.self_heal_max_consecutive_rounds == 9),
            "并发更新不得撕裂读写结果: interval={} rounds={}",
            persisted.self_heal_min_interval_secs,
            persisted.self_heal_max_consecutive_rounds
        );
        assert!(!dir.join("config.json.tmp").exists(), "不得残留临时文件");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
