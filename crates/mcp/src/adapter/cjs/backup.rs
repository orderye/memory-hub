//! 创界山备份清单透明联动 (BackupManifestCoordinator)
//!
//! 原则：核心绝不引入 `.cjsbak` 的 zip 文件解析依赖。
//! 通过在 `BackupRecord.note` 中以 `[cjs_manifest]<json>` 结构化持久化客户端备份清单摘要与 SHA256，
//! 并在 `memory_restore` 时提供双向一致性校验。

use super::fingerprint::ChuangjieshanFingerprint;
use crate::tools::ToolCallResult;
use crate::ToolDispatcher;
use common::{AuthCtx, HubError, Result};
use serde_json::Value;
use uuid::Uuid;

pub struct BackupManifestCoordinator;

const MANIFEST_PREFIX: &str = "[cjs_manifest]";

impl BackupManifestCoordinator {
    pub async fn intercept(
        dispatcher: &ToolDispatcher,
        auth: &AuthCtx,
        name: &str,
        arguments: &mut Value,
        fp: &ChuangjieshanFingerprint,
    ) -> Result<Option<ToolCallResult>> {
        if !fp.is_chuangjieshan {
            return Ok(None);
        }

        match name {
            "memory_backup_create" => {
                Self::handle_backup_create(arguments)?;
                Ok(None)
            }
            "memory_restore" => Self::handle_backup_restore(dispatcher, auth, arguments).await,
            _ => Ok(None),
        }
    }

    /// 备份创建：提取 client_manifest 并无侵入地打包到 note 字段
    fn handle_backup_create(arguments: &mut Value) -> Result<()> {
        let obj = match arguments.as_object_mut() {
            Some(o) => o,
            None => return Ok(()),
        };

        if let Some(manifest) = obj.remove("client_manifest") {
            let manifest_str = serde_json::to_string(&manifest).unwrap_or_default();
            let note_tag = format!("{}{}", MANIFEST_PREFIX, manifest_str);

            if let Some(existing_note) = obj.get("note").and_then(Value::as_str) {
                let combined = format!("{} | {}", existing_note, note_tag);
                obj.insert("note".to_string(), serde_json::json!(combined));
            } else {
                obj.insert("note".to_string(), serde_json::json!(note_tag));
            }
        }

        Ok(())
    }

    /// 备份还原：若携带 cjsbak_sha256，进行防篡改校验和比对
    async fn handle_backup_restore(
        dispatcher: &ToolDispatcher,
        auth: &AuthCtx,
        arguments: &mut Value,
    ) -> Result<Option<ToolCallResult>> {
        let obj = match arguments.as_object_mut() {
            Some(o) => o,
            None => return Ok(None),
        };

        let expected_hash = obj
            .remove("cjsbak_sha256")
            .and_then(|v| v.as_str().map(str::to_string));

        let expected_hash = match expected_hash {
            Some(h) if !h.trim().is_empty() => h.trim().to_string(),
            _ => return Ok(None),
        };

        let backup_id = match obj.get("backup_id").and_then(Value::as_str) {
            Some(id_str) => match Uuid::parse_str(id_str) {
                Ok(id) => id,
                Err(_) => return Ok(None), // 让底层统一抛出参数不合法
            },
            None => return Ok(None),
        };

        // 查询当前快照的 note 元数据
        use mem_core::BackupStore;
        let scoped = mem_core::AccountScopedStore::new(dispatcher.store.clone(), auth.account_id);
        let record = match scoped.get_backup(backup_id).await {
            Ok(rec) => rec,
            Err(e) => return Ok(Some(ToolCallResult::error(&e))),
        };

        if let Some(note) = &record.note {
            if let Some(idx) = note.find(MANIFEST_PREFIX) {
                let json_slice = &note[idx + MANIFEST_PREFIX.len()..];
                let manifest_json = json_slice.split(" | ").next().unwrap_or(json_slice);
                if let Ok(parsed) = serde_json::from_str::<Value>(manifest_json) {
                    if let Some(actual_hash) = parsed.get("cjsbak_sha256").and_then(Value::as_str) {
                        if !actual_hash.eq_ignore_ascii_case(&expected_hash) {
                            return Ok(Some(ToolCallResult::error(&HubError::InvalidArgument(
                                format!(
                                    "创界山备份快照哈希不匹配: 期望 {}, 实际快照记录为 {}",
                                    expected_hash, actual_hash
                                ),
                            ))));
                        }
                    }
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_backup_create_packs_manifest_into_note() {
        let mut args = json!({
            "namespace": "w_123_shared",
            "client_manifest": {
                "appVersion": "1.0.4",
                "cjsbak_sha256": "abc123hash"
            }
        });

        BackupManifestCoordinator::handle_backup_create(&mut args).unwrap();

        assert!(args.get("client_manifest").is_none());
        let note = args.get("note").and_then(Value::as_str).unwrap();
        assert!(note.contains("[cjs_manifest]"));
        assert!(note.contains("abc123hash"));
    }
}
