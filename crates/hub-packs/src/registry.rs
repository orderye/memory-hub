//! 规范包注册表与原子热替换实现（基于设计规范 8.7 与 17.1 节）

use arc_swap::ArcSwap;
use common::{HubError, Result, UserId};
use hub_core::{PackRegistry, PackSet};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// 规范包内存注册表（基于 ArcSwap 实现无锁原子热替换与灰度分桶）
pub struct AtomicPackRegistry {
    /// 稳定基线版本
    stable: ArcSwap<PackSet>,
    /// 灰度金丝雀版本与灰度百分比 (0 ~ 100)
    canary: ArcSwap<Option<(Arc<PackSet>, u32)>>,
    /// 预设核心锁预期哈希（防篡改）
    expected_core_hash: String,
}

impl AtomicPackRegistry {
    pub fn new(initial_pack: PackSet, expected_core_hash: impl Into<String>) -> Result<Self> {
        let expected = expected_core_hash.into();
        Self::verify_pack(&initial_pack, &expected)?;

        Ok(Self {
            stable: ArcSwap::from_pointee(initial_pack),
            canary: ArcSwap::from_pointee(None),
            expected_core_hash: expected,
        })
    }

    /// 校验规范包的核心锁完整性（设计规范 8.7 节）
    pub fn verify_pack(pack: &PackSet, expected_core_hash: &str) -> Result<()> {
        if pack.core_hash != expected_core_hash {
            tracing::error!(
                pack_version = %pack.pack_version,
                provided = %pack.core_hash,
                expected = %expected_core_hash,
                "Core lock validation failed! Potential tamper or configuration error."
            );
            return Err(HubError::PolicyBlocked(
                "Core lock validation failed: core rules have been modified or corrupted"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// 设置金丝雀灰度版本
    pub fn set_canary(&self, canary_pack: PackSet, rollout_percent: u32) -> Result<()> {
        Self::verify_pack(&canary_pack, &self.expected_core_hash)?;
        let percent = rollout_percent.min(100);
        self.canary
            .store(Arc::new(Some((Arc::new(canary_pack), percent))));
        Ok(())
    }

    /// 清除金丝雀版本
    pub fn clear_canary(&self) {
        self.canary.store(Arc::new(None));
    }
}

impl PackRegistry for AtomicPackRegistry {
    fn active_for(&self, user: UserId) -> Arc<PackSet> {
        let canary_opt = self.canary.load();
        if let Some((canary_pack, percent)) = canary_opt.as_ref() {
            if *percent > 0 {
                // 计算用户哈希分桶 (0 ~ 99)
                let mut hasher = Sha256::new();
                hasher.update(user.as_bytes());
                let hash = hasher.finalize();
                let bucket = (hash[0] as u32) % 100;

                if bucket < *percent {
                    return Arc::clone(canary_pack);
                }
            }
        }

        self.stable.load_full()
    }

    fn swap(&self, new: PackSet) -> Result<()> {
        Self::verify_pack(&new, &self.expected_core_hash)?;
        self.stable.store(Arc::new(new));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_core_lock_tamper_detection() {
        let expected_hash = "correct_core_hash_123456";
        let valid_pack = PackSet {
            pack_version: "2026.10.1".to_string(),
            core_hash: expected_hash.to_string(),
            raw_content: serde_json::json!({}),
        };

        let registry = AtomicPackRegistry::new(valid_pack, expected_hash)
            .expect("Should initialize with valid hash");

        // 尝试加载被篡改核心锁的规范包
        let tampered_pack = PackSet {
            pack_version: "2026.10.2".to_string(),
            core_hash: "malicious_tampered_hash".to_string(),
            raw_content: serde_json::json!({}),
        };

        let res = registry.swap(tampered_pack);
        assert!(res.is_err(), "Must reject tampered pack set");
    }

    #[test]
    fn test_atomic_swap_and_canary_rollout() {
        let expected_hash = "hash_v1";
        let pack_v1 = PackSet {
            pack_version: "v1".to_string(),
            core_hash: expected_hash.to_string(),
            raw_content: serde_json::json!({"version": 1}),
        };

        let registry = AtomicPackRegistry::new(pack_v1, expected_hash).unwrap();
        let test_user = Uuid::new_v4();

        // 默认读到 v1
        assert_eq!(registry.active_for(test_user).pack_version, "v1");

        // 全量热替换为 v2
        let pack_v2 = PackSet {
            pack_version: "v2".to_string(),
            core_hash: expected_hash.to_string(),
            raw_content: serde_json::json!({"version": 2}),
        };
        registry.swap(pack_v2).unwrap();
        assert_eq!(registry.active_for(test_user).pack_version, "v2");

        // 开启 100% 灰度测试金丝雀包 v3
        let pack_v3 = PackSet {
            pack_version: "v3".to_string(),
            core_hash: expected_hash.to_string(),
            raw_content: serde_json::json!({"version": 3}),
        };
        registry.set_canary(pack_v3, 100).unwrap();
        assert_eq!(registry.active_for(test_user).pack_version, "v3");

        // 清除金丝雀后回退到稳定版 v2
        registry.clear_canary();
        assert_eq!(registry.active_for(test_user).pack_version, "v2");
    }
}
