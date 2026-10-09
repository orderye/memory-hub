//! 内存级存储实现（用于单元测试、离线调试与无数据库环境）
//!
//! 具备完整的用户隔离、级联删除与时间衰减模拟。

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use common::{HubError, Result, UserId};
use hub_core::{
    apply_decay, DecayReport, DeleteTarget, ItemStatus, MemoryItem, MemoryStore, NewMemory,
    StoreQuery,
};
use std::collections::HashMap;
use std::sync::RwLock;
use uuid::Uuid;

#[derive(Default)]
pub struct InMemoryMemoryStore {
    // 按照 (UserId, ItemId) 索引存储
    items: RwLock<HashMap<(UserId, Uuid), MemoryItem>>,
}

impl InMemoryMemoryStore {
    pub fn new() -> Self {
        Self {
            items: RwLock::new(HashMap::new()),
        }
    }

    /// 检查指定用户当前保留的条目总数（用于删除级联验证）
    pub fn count_for_user(&self, user: UserId) -> usize {
        let read = self.items.read().unwrap();
        read.keys().filter(|(u, _)| *u == user).count()
    }
}

#[async_trait]
impl MemoryStore for InMemoryMemoryStore {
    async fn upsert(&self, new_item: NewMemory) -> Result<MemoryItem> {
        let mut write = self.items.write().unwrap();
        let now = Utc::now();
        let id = Uuid::new_v4();

        let item = MemoryItem {
            id,
            user_id: new_item.user_id,
            layer: new_item.layer,
            text: new_item.text,
            provenance: new_item.provenance,
            confidence: new_item.confidence,
            sensitivity: new_item.sensitivity,
            use_policy: new_item.use_policy,
            source_client: new_item.source_client,
            status: ItemStatus::Active,
            ttl: new_item.ttl_seconds.map(Duration::seconds),
            attrs: new_item.attrs,
            created_at: now,
            updated_at: now,
        };

        write.insert((item.user_id, item.id), item.clone());
        Ok(item)
    }

    async fn get(&self, user: UserId, id: Uuid) -> Result<MemoryItem> {
        let read = self.items.read().unwrap();
        read.get(&(user, id)).cloned().ok_or(HubError::NotFound)
    }

    async fn query(&self, q: StoreQuery) -> Result<Vec<MemoryItem>> {
        let read = self.items.read().unwrap();
        let mut results = Vec::new();

        for ((user, _), item) in read.iter() {
            if *user != q.user_id {
                continue;
            }

            if let Some(ref layers) = q.layers {
                if !layers.contains(&item.layer) {
                    continue;
                }
            }

            if let Some(status) = q.status {
                if item.status != status {
                    continue;
                }
            }

            results.push(item.clone());

            if let Some(limit) = q.limit {
                if results.len() >= limit {
                    break;
                }
            }
        }

        Ok(results)
    }

    async fn delete_cascade(&self, user: UserId, target: DeleteTarget) -> Result<u64> {
        let mut write = self.items.write().unwrap();
        let mut deleted_count = 0u64;

        match target {
            DeleteTarget::Single(item_id) => {
                if write.remove(&(user, item_id)).is_some() {
                    deleted_count = 1;
                }
            }
            DeleteTarget::Scope {
                layer,
                source_client,
                all,
            } => {
                let to_remove: Vec<(UserId, Uuid)> = write
                    .iter()
                    .filter(|((u, _), item)| {
                        if *u != user {
                            return false;
                        }
                        if all {
                            return true;
                        }
                        if let Some(target_layer) = layer {
                            if item.layer != target_layer {
                                return false;
                            }
                        }
                        if let Some(ref target_client) = source_client {
                            if item.source_client != *target_client {
                                return false;
                            }
                        }
                        true
                    })
                    .map(|(key, _)| *key)
                    .collect();

                for key in to_remove {
                    write.remove(&key);
                    deleted_count += 1;
                }
            }
        }

        Ok(deleted_count)
    }

    async fn apply_decay(&self, now: DateTime<Utc>) -> Result<DecayReport> {
        let mut write = self.items.write().unwrap();
        let mut decayed_count = 0u64;
        let mut retired_count = 0u64;
        let mut purged_count = 0u64;

        for item in write.values_mut() {
            if item.status != ItemStatus::Active {
                continue;
            }

            // 计算与上次更新的时间间隔
            let delta = now.signed_duration_since(item.updated_at);
            if delta.num_seconds() <= 0 {
                continue;
            }

            let (new_conf, new_status) =
                apply_decay(item.confidence, item.layer, item.provenance, delta);

            if (new_conf - item.confidence).abs() > 1e-4 {
                decayed_count += 1;
                item.confidence = new_conf;
            }

            if new_status == ItemStatus::Retired {
                retired_count += 1;
                item.status = ItemStatus::Retired;
            }

            // 检查 TTL 过期
            if let Some(ttl) = item.ttl {
                if now.signed_duration_since(item.created_at) > ttl {
                    item.status = ItemStatus::Retired;
                    purged_count += 1;
                }
            }
        }

        Ok(DecayReport {
            decayed_count,
            retired_count,
            purged_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_core::{Layer, Provenance, Sensitivity, UsePolicy};

    #[tokio::test]
    async fn test_tenant_isolation() {
        let store = InMemoryMemoryStore::new();
        let user_a = Uuid::new_v4();
        let user_b = Uuid::new_v4();

        // 用户 A 写入
        let item_a = store
            .upsert(NewMemory {
                user_id: user_a,
                layer: Layer::Preference,
                text: "喜欢中文回复".to_string(),
                provenance: Provenance::Stated,
                confidence: 1.0,
                sensitivity: Sensitivity::Normal,
                use_policy: UsePolicy::Free,
                source_client: "client_1".to_string(),
                ttl_seconds: None,
                attrs: serde_json::json!({}),
            })
            .await
            .unwrap();

        // 用户 B 无法获取用户 A 的条目
        let err = store.get(user_b, item_a.id).await;
        assert!(matches!(err, Err(HubError::NotFound)));

        // 用户 B 查询列表为空
        let list_b = store
            .query(StoreQuery {
                user_id: user_b,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(list_b.len(), 0);

        // 用户 A 可以正常获取
        let fetched_a = store.get(user_a, item_a.id).await.unwrap();
        assert_eq!(fetched_a.text, "喜欢中文回复");
    }

    #[tokio::test]
    async fn test_cascade_deletion() {
        let store = InMemoryMemoryStore::new();
        let user_a = Uuid::new_v4();

        // 写入两条记忆
        let m1 = store
            .upsert(NewMemory {
                user_id: user_a,
                layer: Layer::Preference,
                text: "条目 1".to_string(),
                provenance: Provenance::Stated,
                confidence: 1.0,
                sensitivity: Sensitivity::Normal,
                use_policy: UsePolicy::Free,
                source_client: "asst_1".to_string(),
                ttl_seconds: None,
                attrs: serde_json::json!({}),
            })
            .await
            .unwrap();

        let _m2 = store
            .upsert(NewMemory {
                user_id: user_a,
                layer: Layer::Fact,
                text: "条目 2".to_string(),
                provenance: Provenance::Stated,
                confidence: 1.0,
                sensitivity: Sensitivity::Normal,
                use_policy: UsePolicy::Free,
                source_client: "asst_2".to_string(),
                ttl_seconds: None,
                attrs: serde_json::json!({}),
            })
            .await
            .unwrap();

        assert_eq!(store.count_for_user(user_a), 2);

        // 删除单条
        let deleted = store
            .delete_cascade(user_a, DeleteTarget::Single(m1.id))
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(store.count_for_user(user_a), 1);

        // 按范围清空全部
        let deleted_all = store
            .delete_cascade(
                user_a,
                DeleteTarget::Scope {
                    layer: None,
                    source_client: None,
                    all: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(deleted_all, 1);
        assert_eq!(store.count_for_user(user_a), 0);
    }
}
