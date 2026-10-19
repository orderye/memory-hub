# Memory Hub Workspace Rules

## 1. CNB 提交与推送规范
- **提交邮箱**：向 CNB（远程地址匹配 `cnb.cool`）提交代码时，Git commit 邮箱必须统一使用：
  `3WJRP74JzJLZlOYQ8b7VcB+cnb.dgyOUWMigHA@noreply.cnb.cool`
- **推送说明**：每次向 CNB 远程推送时，必须清晰详细地写明更新内容：
  1. 本次更新涉及的文件列表与变更类型；
  2. 新增或修复的核心功能与架构影响；
  3. 数据库迁移（Migration）变更与兼容性说明；
  4. 质量验收与测试运行结果（`cargo test`, `cargo clippy`, `cargo fmt`）；
  5. 包含的 Commit SHA 与提交说明。

## 2. 构建与测试纪律
- 禁止运行 `cargo test --all-features`（路径包含空格会导致 `tikv-jemalloc-sys` C 编译报错）。
- 统一使用 `cargo test --workspace` 与 `cargo clippy --workspace --all-targets -- -D warnings`。
