use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{ipc::Invoke, Manager, Runtime};

const OFFLINE_ERROR: &str = "调试模式：后端连接已断开，请重启应用恢复。";
const SHUTDOWN_COMMAND: &str = "debug_shutdown_backend";

/// Process-local fault injection; a new application process starts online.
#[derive(Default)]
pub struct DebugBackendState {
    offline: AtomicBool,
}

impl DebugBackendState {
    fn simulate_offline(&self) {
        self.offline.store(true, Ordering::SeqCst);
    }

    fn check_command(&self, command: &str) -> Result<(), &'static str> {
        // Keep the exit control usable after disconnecting application IPC.
        if command != SHUTDOWN_COMMAND && self.offline.load(Ordering::SeqCst) {
            Err(OFFLINE_ERROR)
        } else {
            Ok(())
        }
    }
}

/// Gate every application command, including direct invokes from plugins.
/// Tauri's window/event APIs remain available so the WebView can stay open.
pub fn with_backend_gate<R, F>(handler: F) -> impl Fn(Invoke<R>) -> bool + Send + Sync + 'static
where
    R: Runtime,
    F: Fn(Invoke<R>) -> bool + Send + Sync + 'static,
{
    move |invoke| {
        let result = invoke
            .message
            .webview_ref()
            .state::<DebugBackendState>()
            .check_command(invoke.message.command());
        if let Err(error) = result {
            invoke.resolver.reject(error);
            return true;
        }
        handler(invoke)
    }
}

/// Exit the application, or simulate disconnected application IPC while keeping
/// the WebView alive. Offline simulation rejects subsequent application commands;
/// existing SSH connections, in-flight tasks and backend events continue. It is
/// intentionally not a service shutdown, and restarting the app clears the mode.
#[tauri::command]
pub fn debug_shutdown_backend(
    app: tauri::AppHandle,
    state: tauri::State<'_, DebugBackendState>,
    keep_frontend_open: bool,
) -> Result<(), String> {
    if keep_frontend_open {
        state.simulate_offline();
    } else {
        app.exit(0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_process_allows_application_commands() {
        let state = DebugBackendState::default();
        for command in [
            "app_get_bootstrap",
            "ssh_connect",
            "agent_start_task",
            "plugin_reload",
        ] {
            assert_eq!(state.check_command(command), Ok(()));
        }
    }

    #[test]
    fn offline_mode_rejects_application_commands_with_a_recovery_message() {
        let state = DebugBackendState::default();
        state.simulate_offline();
        for command in [
            "app_get_bootstrap",
            "ssh_connect",
            "agent_start_task",
            "plugin_reload",
        ] {
            assert_eq!(state.check_command(command), Err(OFFLINE_ERROR));
        }
        assert_eq!(
            DebugBackendState::default().check_command("ssh_connect"),
            Ok(())
        );
    }

    #[test]
    fn only_the_exit_control_bypasses_offline_mode() {
        let state = DebugBackendState::default();
        state.simulate_offline();
        state.simulate_offline();
        assert_eq!(state.check_command(SHUTDOWN_COMMAND), Ok(()));
        assert_eq!(
            state.check_command("debug_other_command"),
            Err(OFFLINE_ERROR)
        );
    }
}
