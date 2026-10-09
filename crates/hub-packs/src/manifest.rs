//! Skill Manifest 规范与生命周期定义（基于设计规范 8.1 与 8.2 节）

use common::{HubError, Result};
use serde::{Deserialize, Serialize};

/// Skill 分层角色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillLayer {
    /// 规范源：产出带来源和版本的规范条目
    Source,
    /// 统合层：编译场景补丁与冲突消解表
    Compile,
    /// 约束层：公共伦理与个人记忆双向修正
    Constrain,
    /// 仲裁层：冲突检测与调度
    Arbitrate,
}

/// 规则约束强度
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleStrength {
    Hard,
    Soft,
}

/// Skill 挂载钩子
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillHooks {
    #[serde(default)]
    pub refine: bool,
    #[serde(default)]
    pub audit: bool,
}

/// 核心锁定义（设计规范 8.7 节）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoreLockConfig {
    #[serde(default)]
    pub rules: Vec<String>,
}

/// Skill Manifest 结构体（设计规范 8.2 节）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    pub id: String,
    pub name: String,
    pub layer: SkillLayer,
    pub version: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub submodules: Vec<String>,
    #[serde(default)]
    pub hooks: SkillHooks,
    pub default_strength: RuleStrength,
    /// 是否允许卸载（核心锁涉及的内容必须为 false）
    pub unloadable: bool,
    #[serde(default)]
    pub core_lock: CoreLockConfig,
}

impl SkillManifest {
    /// 基础依赖与核心锁合法性校验
    pub fn validate(&self) -> Result<()> {
        // 核心锁规则包含条目时，必须为不可卸载
        if !self.core_lock.rules.is_empty() && self.unloadable {
            return Err(HubError::PolicyBlocked(format!(
                "Skill {} contains core lock rules but is marked as unloadable",
                self.id
            )));
        }
        Ok(())
    }
}

/// 加载系统预置的六大 Skill 定义清单
pub fn get_canonical_skills() -> Vec<SkillManifest> {
    vec![
        // 1. 天心即民心（约束层，含核心锁硬规则）
        SkillManifest {
            id: "tianxin".to_string(),
            name: "天心即民心".to_string(),
            layer: SkillLayer::Constrain,
            version: "1.0.0".to_string(),
            depends_on: vec![],
            submodules: vec!["chuantong_daode".to_string(), "hegui_dixian".to_string()],
            hooks: SkillHooks {
                refine: false,
                audit: true,
            },
            default_strength: RuleStrength::Hard,
            unloadable: false, // 核心锁不可卸载
            core_lock: CoreLockConfig {
                rules: vec!["tx-core-0001".to_string()],
            },
        },
        // 2. 天理与人欲（规范源，含差序格局、人情面子、礼貌准则）
        SkillManifest {
            id: "tianli_renyu".to_string(),
            name: "天理与人欲".to_string(),
            layer: SkillLayer::Source,
            version: "1.0.0".to_string(),
            depends_on: vec![],
            submodules: vec![
                "tianli".to_string(),
                "renyu".to_string(),
                "chaxu".to_string(),
                "renqing".to_string(),
                "limao".to_string(),
            ],
            hooks: SkillHooks {
                refine: true,
                audit: true,
            },
            default_strength: RuleStrength::Soft,
            unloadable: true,
            core_lock: CoreLockConfig::default(),
        },
        // 3. 我命即天命（约束层，按个人记忆修正）
        SkillManifest {
            id: "woming".to_string(),
            name: "我命即天命".to_string(),
            layer: SkillLayer::Constrain,
            version: "1.0.0".to_string(),
            depends_on: vec![],
            submodules: vec!["geren_pianhao".to_string(), "anti_sycophancy".to_string()],
            hooks: SkillHooks {
                refine: true,
                audit: true,
            },
            default_strength: RuleStrength::Soft,
            unloadable: true,
            core_lock: CoreLockConfig::default(),
        },
        // 4. 知守与知行（统合层，编译冲突消解表）
        SkillManifest {
            id: "zhishou".to_string(),
            name: "知守与知行".to_string(),
            layer: SkillLayer::Compile,
            version: "1.0.0".to_string(),
            depends_on: vec!["tianli_renyu".to_string()],
            submodules: vec![
                "zhishou_dixian".to_string(),
                "zhixing_changjing".to_string(),
            ],
            hooks: SkillHooks {
                refine: true,
                audit: false,
            },
            default_strength: RuleStrength::Soft,
            unloadable: true,
            core_lock: CoreLockConfig::default(),
        },
        // 5. 如梦亦如幻（仲裁层，冲突检测与调度）
        SkillManifest {
            id: "rumeng".to_string(),
            name: "如梦亦如幻".to_string(),
            layer: SkillLayer::Arbitrate,
            version: "1.0.0".to_string(),
            depends_on: vec![
                "tianxin".to_string(),
                "woming".to_string(),
                "zhishou".to_string(),
            ],
            submodules: vec!["zhongcai_diaodu".to_string()],
            hooks: SkillHooks {
                refine: true,
                audit: true,
            },
            default_strength: RuleStrength::Soft,
            unloadable: true,
            core_lock: CoreLockConfig::default(),
        },
        // 6. 国民总记忆（规范源，重大历史事件口径，默认关闭）
        SkillManifest {
            id: "guomin".to_string(),
            name: "国民总记忆".to_string(),
            layer: SkillLayer::Source,
            version: "1.0.0".to_string(),
            depends_on: vec![],
            submodules: vec!["shishi_ceng".to_string(), "biaoshu_ceng".to_string()],
            hooks: SkillHooks {
                refine: true,
                audit: true,
            },
            default_strength: RuleStrength::Soft,
            unloadable: true,
            core_lock: CoreLockConfig::default(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonical_skills_validation() {
        let skills = get_canonical_skills();
        assert_eq!(skills.len(), 6);

        for skill in &skills {
            assert!(skill.validate().is_ok());
        }

        // 天心即民心必须为不可卸载核心 Skill
        let tianxin = skills.iter().find(|s| s.id == "tianxin").unwrap();
        assert!(!tianxin.unloadable);
        assert!(!tianxin.core_lock.rules.is_empty());
    }

    #[test]
    fn test_invalid_skill_core_lock() {
        let invalid_manifest = SkillManifest {
            id: "invalid_test".to_string(),
            name: "非法测试".to_string(),
            layer: SkillLayer::Source,
            version: "1.0.0".to_string(),
            depends_on: vec![],
            submodules: vec![],
            hooks: SkillHooks::default(),
            default_strength: RuleStrength::Hard,
            unloadable: true, // 错误：有核心锁却标记可卸载
            core_lock: CoreLockConfig {
                rules: vec!["r1".to_string()],
            },
        };

        assert!(invalid_manifest.validate().is_err());
    }
}
