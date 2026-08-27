//! 模型映射（请求时模型名转发）
//!
//! 允许把客户端请求里的模型名（如 `gpt-5.5`）在请求时改写为后端实际使用的
//! 模型名（如 `claude-opus-4.8`）。用途：让 Codex / OpenAI SDK 客户端用它们
//! 自己的模型名（gpt-5.5 / gpt-5.4 等）请求，服务端透明转发到 Claude 模型。
//!
//! 关键约束：
//! - 映射的**源名**（gpt-5.5 等）**不出现在 `/v1/models` 列表**里，仅在请求命中时转发。
//! - 映射表持久化到 `model_mappings.json`（与 credentials.json 同目录）。
//! - 匹配大小写不敏感（源名归一化为小写存储与查询）。
//!
//! 设计参考 `groups.rs` 的 RwLock + JSON 持久化模式。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// 单条模型映射（持久化实体 / API 出入参）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelMapping {
    /// 源模型名（客户端请求里出现的名字，如 `gpt-5.5`）
    pub source: String,
    /// 目标模型名（转发到后端实际使用的名字，如 `claude-opus-4.8`）
    pub target: String,
}

/// 默认内置映射：gpt-5.5 / gpt-5.4 → claude-opus-4.8
const DEFAULT_MAPPINGS: &[(&str, &str)] = &[
    ("gpt-5.5", "claude-opus-4.8"),
    ("gpt-5.4", "claude-opus-4.8"),
];

/// 模型映射管理器（线程安全 + 自动持久化）
pub struct ModelMappingManager {
    inner: RwLock<Inner>,
    path: Option<PathBuf>,
}

struct Inner {
    /// key = 源名的小写归一化形式；value = 保留原始大小写的映射条目
    entries: HashMap<String, ModelMapping>,
}

fn norm(source: &str) -> String {
    source.trim().to_ascii_lowercase()
}

impl ModelMappingManager {
    /// 空管理器（无持久化，供测试）
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                entries: HashMap::new(),
            }),
            path: None,
        }
    }

    /// 从 `model_mappings.json` 加载；文件不存在时用内置默认映射初始化并落盘。
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let list: Vec<ModelMapping> = if content.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&content)?
            };
            let mut entries = HashMap::with_capacity(list.len());
            for m in list {
                let key = norm(&m.source);
                if !key.is_empty() {
                    entries.insert(key, m);
                }
            }
            Ok(Self {
                inner: RwLock::new(Inner { entries }),
                path: Some(path),
            })
        } else {
            // 首次启动：写入内置默认映射
            let mut entries = HashMap::new();
            for (src, tgt) in DEFAULT_MAPPINGS {
                entries.insert(
                    norm(src),
                    ModelMapping {
                        source: src.to_string(),
                        target: tgt.to_string(),
                    },
                );
            }
            let mgr = Self {
                inner: RwLock::new(Inner { entries }),
                path: Some(path),
            };
            mgr.save_locked(&mgr.inner.read());
            Ok(mgr)
        }
    }

    /// 解析源模型名 → 目标模型名（命中返回 Some(target)，未命中 None）。
    /// 大小写不敏感。
    pub fn resolve(&self, model: &str) -> Option<String> {
        let key = norm(model);
        if key.is_empty() {
            return None;
        }
        self.inner
            .read()
            .entries
            .get(&key)
            .map(|m| m.target.clone())
    }

    /// 列出全部映射（按源名排序，输出稳定）
    pub fn list(&self) -> Vec<ModelMapping> {
        let mut out: Vec<ModelMapping> = self.inner.read().entries.values().cloned().collect();
        out.sort_by(|a, b| {
            a.source
                .to_ascii_lowercase()
                .cmp(&b.source.to_ascii_lowercase())
        });
        out
    }

    /// 新增或更新一条映射。source/target 去除首尾空白；两者皆非空才生效。
    /// 返回规范化后的条目；source 为空返回错误。
    pub fn upsert(&self, source: &str, target: &str) -> anyhow::Result<ModelMapping> {
        let source = source.trim().to_string();
        let target = target.trim().to_string();
        if source.is_empty() {
            anyhow::bail!("源模型名不能为空");
        }
        if target.is_empty() {
            anyhow::bail!("目标模型名不能为空");
        }
        let entry = ModelMapping {
            source: source.clone(),
            target,
        };
        let mut inner = self.inner.write();
        inner.entries.insert(norm(&source), entry.clone());
        self.save_locked(&inner);
        Ok(entry)
    }

    /// 删除一条映射（按源名，大小写不敏感）。返回是否删除了条目。
    pub fn remove(&self, source: &str) -> bool {
        let mut inner = self.inner.write();
        let removed = inner.entries.remove(&norm(source)).is_some();
        if removed {
            self.save_locked(&inner);
        }
        removed
    }

    /// 整表替换（供前端一次性保存全部映射）。忽略 source 为空的条目。
    pub fn replace_all(&self, mappings: Vec<ModelMapping>) {
        let mut entries = HashMap::with_capacity(mappings.len());
        for m in mappings {
            let source = m.source.trim().to_string();
            let target = m.target.trim().to_string();
            if source.is_empty() || target.is_empty() {
                continue;
            }
            entries.insert(norm(&source), ModelMapping { source, target });
        }
        let mut inner = self.inner.write();
        inner.entries = entries;
        self.save_locked(&inner);
    }

    fn save_locked(&self, inner: &Inner) {
        let Some(path) = &self.path else {
            return;
        };
        let mut list: Vec<&ModelMapping> = inner.entries.values().collect();
        list.sort_by(|a, b| {
            a.source
                .to_ascii_lowercase()
                .cmp(&b.source.to_ascii_lowercase())
        });
        match serde_json::to_string_pretty(&list) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("写入模型映射失败 {}: {}", path.display(), e);
                }
            }
            Err(e) => tracing::warn!("序列化模型映射失败: {}", e),
        }
    }
}

impl Default for ModelMappingManager {
    fn default() -> Self {
        Self::new()
    }
}

/// `model_mappings.json` 的默认路径（与凭据同目录）
pub fn default_path_in(dir: &Path) -> PathBuf {
    dir.join("model_mappings.json")
}

pub type SharedModelMappingManager = Arc<ModelMappingManager>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_is_case_insensitive_and_seeded_default() {
        let mgr = ModelMappingManager::new();
        mgr.upsert("gpt-5.5", "claude-opus-4.8").unwrap();
        assert_eq!(mgr.resolve("gpt-5.5").as_deref(), Some("claude-opus-4.8"));
        assert_eq!(mgr.resolve("GPT-5.5").as_deref(), Some("claude-opus-4.8"));
        assert_eq!(mgr.resolve(" gpt-5.5 ").as_deref(), Some("claude-opus-4.8"));
        assert_eq!(mgr.resolve("gpt-4o"), None);
    }

    #[test]
    fn upsert_rejects_empty_and_updates_in_place() {
        let mgr = ModelMappingManager::new();
        assert!(mgr.upsert("", "x").is_err());
        assert!(mgr.upsert("gpt-5.5", "").is_err());
        mgr.upsert("gpt-5.4", "claude-opus-4.8").unwrap();
        mgr.upsert("gpt-5.4", "claude-sonnet-4.5").unwrap();
        assert_eq!(mgr.list().len(), 1);
        assert_eq!(mgr.resolve("gpt-5.4").as_deref(), Some("claude-sonnet-4.5"));
    }

    #[test]
    fn remove_and_replace_all() {
        let mgr = ModelMappingManager::new();
        mgr.upsert("a", "1").unwrap();
        mgr.upsert("b", "2").unwrap();
        assert!(mgr.remove("A"));
        assert!(!mgr.remove("A"));
        assert_eq!(mgr.list().len(), 1);
        mgr.replace_all(vec![
            ModelMapping {
                source: "x".into(),
                target: "10".into(),
            },
            ModelMapping {
                source: "  ".into(),
                target: "skip".into(),
            },
        ]);
        assert_eq!(mgr.list().len(), 1);
        assert_eq!(mgr.resolve("x").as_deref(), Some("10"));
    }

    #[test]
    fn load_seeds_defaults_when_file_absent() {
        let dir = std::env::temp_dir().join(format!("mm_test_{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = default_path_in(&dir);
        let mgr = ModelMappingManager::load(&path).unwrap();
        assert_eq!(mgr.resolve("gpt-5.5").as_deref(), Some("claude-opus-4.8"));
        assert_eq!(mgr.resolve("gpt-5.4").as_deref(), Some("claude-opus-4.8"));
        assert!(path.exists());
        // 重新加载应读回持久化内容
        let mgr2 = ModelMappingManager::load(&path).unwrap();
        assert_eq!(mgr2.resolve("gpt-5.5").as_deref(), Some("claude-opus-4.8"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
