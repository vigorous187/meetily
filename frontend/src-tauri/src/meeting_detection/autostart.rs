use serde::Serialize;
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

pub(crate) const LAUNCH_AT_LOGIN_CONFIGURED: &str = "launch_at_login_configured";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchAtLoginStatus {
    pub enabled: bool,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub message: String,
}

fn unavailable(error_code: &str, message: &str) -> LaunchAtLoginStatus {
    LaunchAtLoginStatus {
        enabled: false,
        available: false,
        error_code: Some(error_code.to_string()),
        message: message.to_string(),
    }
}

#[tauri::command]
pub fn get_launch_at_login_status<R: Runtime>(app: AppHandle<R>) -> LaunchAtLoginStatus {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        use tauri_plugin_autostart::ManagerExt;
        match app.autolaunch().is_enabled() {
            Ok(enabled) => LaunchAtLoginStatus {
                enabled,
                available: true,
                error_code: None,
                message: if enabled {
                    "Meetily Plus will launch hidden at login."
                } else {
                    "Launch at login is disabled. Automatic capture works only while Meetily Plus is running."
                }
                .to_string(),
            },
            Err(_) => unavailable(
                "launch_at_login_status_failed",
                "Launch-at-login status could not be read.",
            ),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = app;
        unavailable(
            "launch_at_login_unavailable",
            "Launch at login is unavailable on this platform.",
        )
    }
}

#[tauri::command]
pub fn set_launch_at_login<R: Runtime>(
    app: AppHandle<R>,
    enabled: bool,
) -> LaunchAtLoginStatus {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        use tauri_plugin_autostart::ManagerExt;
        let manager = app.autolaunch();
        let result = if enabled {
            manager.enable()
        } else {
            manager.disable()
        };
        if result.is_err() {
            return unavailable(
                "launch_at_login_update_failed",
                "Launch-at-login could not be updated.",
            );
        }
        if let Ok(store) = app.store("preferences.json") {
            store.set(LAUNCH_AT_LOGIN_CONFIGURED, serde_json::Value::Bool(true));
            let _ = store.save();
        }
        get_launch_at_login_status(app)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (app, enabled);
        unavailable(
            "launch_at_login_unavailable",
            "Launch at login is unavailable on this platform.",
        )
    }
}

pub fn launched_in_background() -> bool {
    std::env::args_os().any(|argument| argument == "--background")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_status_has_stable_error_code() {
        let status = unavailable("launch_at_login_test", "test");
        assert!(!status.available);
        assert_eq!(status.error_code.as_deref(), Some("launch_at_login_test"));
    }
}
