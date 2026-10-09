#![forbid(unsafe_code)]

//! 用户中心与同意授权页服务端渲染（设计规范 17.4 与 18.2 WP-12 节）
//!
//! 提供：
//! 1. 单独同意页渲染（展示读写层、敏感层独立授权与条款回执哈希）
//! 2. 用户控制中心页面（已授权助理列表、一键撤销授权）
//! 3. 个人记忆导出页面与一键注销操作

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// 助理授权记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizedClient {
    pub client_id: String,
    pub client_name: String,
    pub scopes: Vec<String>,
    pub allow_sensitive: bool,
    pub authorized_at: DateTime<Utc>,
    pub consent_receipt_hash: String,
}

/// 用户中心管理状态机
pub struct UserCenterService {
    /// 模拟用户授权记录 (user_id -> List<AuthorizedClient>)
    authorizations: Arc<Mutex<HashMap<Uuid, Vec<AuthorizedClient>>>>,
}

impl Default for UserCenterService {
    fn default() -> Self {
        Self::new()
    }
}

impl UserCenterService {
    pub fn new() -> Self {
        Self {
            authorizations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 记录用户同意并生成回执哈希
    pub fn record_consent(
        &self,
        user_id: Uuid,
        client_id: &str,
        client_name: &str,
        scopes: &[String],
        allow_sensitive: bool,
    ) -> AuthorizedClient {
        let now = Utc::now();
        // 计算不可篡改的同意回执哈希 SHA-256
        let raw_payload = format!(
            "user={}&client={}&scopes={:?}&sensitive={}&time={}",
            user_id, client_id, scopes, allow_sensitive, now
        );
        let receipt_hash = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(raw_payload.as_bytes())
        );

        let client = AuthorizedClient {
            client_id: client_id.to_string(),
            client_name: client_name.to_string(),
            scopes: scopes.to_vec(),
            allow_sensitive,
            authorized_at: now,
            consent_receipt_hash: receipt_hash,
        };

        let mut map = self.authorizations.lock().unwrap();
        let list = map.entry(user_id).or_default();
        list.retain(|c| c.client_id != client_id);
        list.push(client.clone());

        client
    }

    /// 撤销指定助理的授权
    pub fn revoke_consent(&self, user_id: Uuid, client_id: &str) -> bool {
        let mut map = self.authorizations.lock().unwrap();
        if let Some(list) = map.get_mut(&user_id) {
            let initial_len = list.len();
            list.retain(|c| c.client_id != client_id);
            list.len() < initial_len
        } else {
            false
        }
    }

    /// 获取用户的所有已授权助理
    pub fn list_authorizations(&self, user_id: Uuid) -> Vec<AuthorizedClient> {
        self.authorizations
            .lock()
            .unwrap()
            .get(&user_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 渲染独立同意授权页面 HTML
    pub fn render_consent_page(
        client_id: &str,
        client_name: &str,
        requested_scopes: &[String],
    ) -> String {
        let scopes_html: String = requested_scopes
            .iter()
            .map(|s| format!("<li><label><input type='checkbox' checked disabled /> <code>{}</code></label></li>", s))
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <title>记忆中枢 - 授权确认</title>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #f8fafc; padding: 40px; color: #1e293b; }}
        .card {{ max-width: 520px; margin: 0 auto; background: white; padding: 32px; border-radius: 12px; box-shadow: 0 4px 6px -1px rgb(0 0 0 / 0.1); border: 1px solid #e2e8f0; }}
        h2 {{ margin-top: 0; color: #0f172a; font-size: 20px; }}
        .badge {{ background: #e0f2fe; color: #0369a1; padding: 4px 8px; border-radius: 4px; font-size: 13px; font-weight: 600; }}
        .scope-list {{ padding-left: 20px; line-height: 1.8; }}
        .sensitive-box {{ background: #fff1f2; border: 1px solid #fecdd3; padding: 12px; border-radius: 8px; margin: 20px 0; }}
        .btn-group {{ display: flex; gap: 12px; margin-top: 24px; }}
        button {{ flex: 1; padding: 10px 16px; border-radius: 6px; font-weight: 500; cursor: pointer; border: none; }}
        .btn-primary {{ background: #2563eb; color: white; }}
        .btn-secondary {{ background: #e2e8f0; color: #475569; }}
    </style>
</head>
<body>
    <div class="card">
        <h2>助理授权申请</h2>
        <p>第三方应用 <span class="badge">{}</span> 申请访问您的记忆中枢服务。</p>
        
        <h3>申请权限范围：</h3>
        <ul class="scope-list">
            {}
        </ul>

        <div class="sensitive-box">
            <label>
                <input type="checkbox" id="allow_sensitive" name="allow_sensitive" />
                <strong>允许读取敏感层记忆</strong>（健康、财务倾向等，需单独勾选）
            </label>
        </div>

        <p style="font-size: 12px; color: #64748b;">
            点击“同意”即代表您已阅读并同意 <a href="#privacy">《记忆中枢个人信息处理协议》</a>。授权后您可随时在用户控制中心撤销该助理权限。
        </p>

        <form method="POST" action="/oauth/consent/submit">
            <input type="hidden" name="client_id" value="{}" />
            <div class="btn-group">
                <button type="button" class="btn-secondary" onclick="window.close()">拒绝</button>
                <button type="submit" class="btn-primary">同意并授权</button>
            </div>
        </form>
    </div>
</body>
</html>"##,
            client_name, scopes_html, client_id
        )
    }

    /// 渲染用户控制中心页面 HTML（助理管理、记忆导出、撤销授权）
    pub fn render_user_center_page(user_id: Uuid, authorized: &[AuthorizedClient]) -> String {
        let clients_html: String = if authorized.is_empty() {
            "<tr><td colspan='5' style='text-align:center; color:#94a3b8; padding: 24px;'>暂无已授权助理</td></tr>".to_string()
        } else {
            authorized
                .iter()
                .map(|c| {
                    format!(
                        r#"<tr>
                            <td><strong>{}</strong><br><small style="color:#64748b">ID: {}</small></td>
                            <td><code>{}</code></td>
                            <td>{}</td>
                            <td><span style="font-family:monospace; font-size:11px;" title="{}">{}...</span></td>
                            <td>
                                <form method="POST" action="/user/revoke" style="margin:0;">
                                    <input type="hidden" name="client_id" value="{}" />
                                    <button type="submit" class="btn-danger">撤销授权</button>
                                </form>
                            </td>
                        </tr>"#,
                        c.client_name,
                        c.client_id,
                        c.scopes.join(", "),
                        if c.allow_sensitive { "<span style='color:#dc2626'>允许</span>" } else { "禁止" },
                        c.consent_receipt_hash,
                        &c.consent_receipt_hash[0..12],
                        c.client_id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        format!(
            r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
    <meta charset="UTF-8">
    <title>记忆中枢 - 用户控制中心</title>
    <style>
        body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #f8fafc; padding: 40px; color: #1e293b; }}
        .container {{ max-width: 900px; margin: 0 auto; }}
        .header {{ display: flex; justify-content: space-between; align-items: center; margin-bottom: 24px; }}
        .card {{ background: white; padding: 24px; border-radius: 12px; box-shadow: 0 4px 6px -1px rgb(0 0 0 / 0.1); border: 1px solid #e2e8f0; margin-bottom: 24px; }}
        table {{ width: 100%; border-collapse: collapse; text-align: left; }}
        th, td {{ padding: 12px 16px; border-bottom: 1px solid #e2e8f0; font-size: 14px; }}
        th {{ background: #f1f5f9; color: #475569; font-weight: 600; }}
        .btn-danger {{ background: #ef4444; color: white; border: none; padding: 6px 12px; border-radius: 4px; cursor: pointer; }}
        .btn-export {{ background: #0284c7; color: white; border: none; padding: 8px 16px; border-radius: 6px; cursor: pointer; text-decoration: none; display: inline-block; }}
    </style>
</head>
<body>
    <div class="container">
        <div class="header">
            <div>
                <h1 style="margin:0; font-size:24px;">记忆中枢用户控制中心</h1>
                <p style="margin:4px 0 0; color:#64748b; font-size:13px;">用户主体 ID: {}</p>
            </div>
            <div>
                <a href="/user/export?format=json" class="btn-export">导出全部记忆 (JSON)</a>
            </div>
        </div>

        <div class="card">
            <h3 style="margin-top:0;">已连接助理应用</h3>
            <table>
                <thead>
                    <tr>
                        <th>助理名称</th>
                        <th>授权范围</th>
                        <th>敏感层权限</th>
                        <th>同意回执凭据</th>
                        <th>操作</th>
                    </tr>
                </thead>
                <tbody>
                    {}
                </tbody>
            </table>
        </div>
    </div>
</body>
</html>"#,
            user_id, clients_html
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_revoke_consent() {
        let service = UserCenterService::new();
        let user_id = Uuid::new_v4();

        let client = service.record_consent(
            user_id,
            "assistant_x",
            "测试小助理",
            &["memory:context".to_string(), "memory:write".to_string()],
            true,
        );

        assert_eq!(client.client_id, "assistant_x");
        assert_eq!(client.consent_receipt_hash.len(), 64); // SHA-256

        let list = service.list_authorizations(user_id);
        assert_eq!(list.len(), 1);

        let revoked = service.revoke_consent(user_id, "assistant_x");
        assert!(revoked);

        let empty_list = service.list_authorizations(user_id);
        assert!(empty_list.is_empty());
    }

    #[test]
    fn test_render_html_templates() {
        let html_consent = UserCenterService::render_consent_page(
            "client_a",
            "知识助手",
            &["memory:context".to_string()],
        );
        assert!(html_consent.contains("知识助手"));
        assert!(html_consent.contains("memory:context"));

        let user_id = Uuid::new_v4();
        let html_uc = UserCenterService::render_user_center_page(user_id, &[]);
        assert!(html_uc.contains(&user_id.to_string()));
        assert!(html_uc.contains("暂无已授权助理"));
    }
}
