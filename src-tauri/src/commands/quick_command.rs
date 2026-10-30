use tauri::State;
use uuid::Uuid;

use crate::config::quick_commands::{QuickCommand, QuickCommandInput, QuickCommandPatch};
use crate::error::AppError;
use crate::AppState;

#[tauri::command]
pub async fn quick_command_list(
    state: State<'_, AppState>,
    session_key: Option<String>,
) -> Result<Vec<QuickCommand>, AppError> {
    let store = state.quick_command_store.read().await;
    Ok(store.list_for_session(session_key.as_deref()))
}

#[tauri::command]
pub async fn quick_command_add(
    state: State<'_, AppState>,
    command: QuickCommandInput,
) -> Result<QuickCommand, AppError> {
    let created = state
        .quick_command_store
        .mutate(|store| {
            let id = Uuid::new_v4().to_string();
            store.add(id, command)
        })
        .await?;

    Ok(created)
}

#[tauri::command]
pub async fn quick_command_update(
    state: State<'_, AppState>,
    id: String,
    patch: QuickCommandPatch,
) -> Result<(), AppError> {
    state
        .quick_command_store
        .mutate(|store| store.update(&id, patch))
        .await
}

#[tauri::command]
pub async fn quick_command_delete(state: State<'_, AppState>, id: String) -> Result<(), AppError> {
    state
        .quick_command_store
        .mutate(|store| {
            if !store.remove(&id) {
                return Err(AppError::Config(format!("未找到快捷指令: {}", id)));
            }
            Ok(())
        })
        .await
}
