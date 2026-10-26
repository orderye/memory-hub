use super::*;
use super::dispatch::stdio_local_auth_ctx;

/// stdio 桥接主循环：读一行、回一帧
pub async fn run_stdio(state: Arc<McpTransportState>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // 身份在会话开始时确定一次：中途改环境变量不应悄悄提升权限。
    // 是否允许本地代理取决于部署模式（只有 lite 允许）。
    let local_auth = stdio_local_auth_ctx(state.capabilities.deployment_profile.as_str())?;

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| HubError::Internal(anyhow::anyhow!("stdin 读取失败: {}", e)))?
    {
        if let Some(resp) = handle_raw_frame(&state, &line, local_auth.as_ref()).await {
            let payload = serde_json::to_string(&resp)
                .map_err(|e| HubError::Internal(anyhow::anyhow!("响应序列化失败: {}", e)))?;
            stdout
                .write_all(payload.as_bytes())
                .await
                .map_err(|e| HubError::Internal(anyhow::anyhow!("stdout 写入失败: {}", e)))?;
            stdout
                .write_all(b"\n")
                .await
                .map_err(|e| HubError::Internal(anyhow::anyhow!("stdout 写入失败: {}", e)))?;
            stdout
                .flush()
                .await
                .map_err(|e| HubError::Internal(anyhow::anyhow!("stdout 刷新失败: {}", e)))?;
        }
    }
    Ok(())
}

