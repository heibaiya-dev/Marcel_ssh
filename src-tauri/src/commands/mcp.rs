use serde::Serialize;
use tauri::State;

use crate::error::AppError;
use crate::mcp::manager::McpServerRuntimeStatus;
use crate::mcp::protocol::McpToolInfo;
use crate::mcp::store::{McpServerConfig, McpServerInput};
use crate::AppState;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerListResponse {
    pub servers: Vec<McpServerConfig>,
    pub statuses: Vec<McpServerRuntimeStatus>,
}

#[tauri::command]
pub async fn mcp_list_servers(
    state: State<'_, AppState>,
) -> Result<McpServerListResponse, AppError> {
    let servers = state.mcp_store.read().await.list().to_vec();
    let statuses = state.mcp_manager.statuses(&servers).await;
    Ok(McpServerListResponse { servers, statuses })
}

#[tauri::command]
pub async fn mcp_add_server(
    state: State<'_, AppState>,
    input: McpServerInput,
) -> Result<McpServerConfig, AppError> {
    let server = McpServerConfig::new(input)?;
    let cloned = server.clone();
    state
        .mcp_store
        .mutate(|store| {
            store.add(server);
            Ok(())
        })
        .await?;

    Ok(cloned)
}

#[tauri::command]
pub async fn mcp_update_server(
    state: State<'_, AppState>,
    id: String,
    input: McpServerInput,
) -> Result<(), AppError> {
    state
        .mcp_store
        .mutate(|store| store.update(&id, input))
        .await?;
    // 落盘之后才清缓存：写盘失败时配置没变，缓存也该留着。
    state.mcp_manager.clear_cache(&id).await;

    Ok(())
}

#[tauri::command]
pub async fn mcp_delete_server(state: State<'_, AppState>, id: String) -> Result<(), AppError> {
    state.mcp_store.mutate(|store| store.delete(&id)).await?;
    state.mcp_manager.clear_cache(&id).await;

    Ok(())
}

#[tauri::command]
pub async fn mcp_toggle_server(state: State<'_, AppState>, id: String) -> Result<(), AppError> {
    state.mcp_store.mutate(|store| store.toggle(&id)).await?;
    // enabled 不进 tools cache key；Agent 只注册 store 里 enabled 的 server。
    // 不清 cache，避免关/开闪空与无意义重刷。

    Ok(())
}

#[tauri::command]
pub async fn mcp_refresh_tools(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<McpToolInfo>, AppError> {
    let server = {
        let store = state.mcp_store.read().await;
        store
            .get(&id)
            .cloned()
            .ok_or_else(|| AppError::Config(format!("MCP server not found: {}", id)))?
    };
    state.mcp_manager.refresh_tools(&server).await
}

#[tauri::command]
pub async fn mcp_call_tool(
    state: State<'_, AppState>,
    id: String,
    tool_name: String,
    arguments: serde_json::Value,
) -> Result<String, AppError> {
    let server = {
        let store = state.mcp_store.read().await;
        store
            .get(&id)
            .cloned()
            .ok_or_else(|| AppError::Config(format!("MCP server not found: {}", id)))?
    };
    state
        .mcp_manager
        .call_tool(&server, &tool_name, arguments)
        .await
}
