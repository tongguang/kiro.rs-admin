//! Admin JSON 文件的原子落盘（同目录 tmp + rename）。

use std::path::Path;

/// 同目录临时文件 + rename 原子替换。失败时清理 tmp，避免半截文件残留。
pub(crate) fn atomic_write(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let write_result = std::fs::write(&tmp, contents).and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_target_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!(
            "kiro-atomic-write-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.json");
        atomic_write(&path, b"{\"a\":1}").unwrap();
        atomic_write(&path, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
