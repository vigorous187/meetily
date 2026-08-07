pub fn format_timestamp(seconds: f64) -> String {
    let total_seconds = seconds as u64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let secs = total_seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, secs)
}

/// Opens macOS System Settings to a specific privacy preference pane
#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn open_system_settings(preference_pane: Option<String>) -> Result<(), String> {
    use std::process::Command;

    let preference_pane = match preference_pane.as_deref().unwrap_or("") {
        "" | "Privacy_Microphone" => "Privacy_Microphone",
        "Privacy_ScreenCapture" => "Privacy_ScreenCapture",
        _ => return Err("Unsupported System Settings pane".to_string()),
    };

    // Construct the URL for System Settings
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{}", preference_pane);

    // Use the 'open' command on macOS to open the URL
    Command::new("open")
        .arg("--")
        .arg(&url)
        .spawn()
        .map_err(|e| format!("Failed to open system settings: {}", e))?;

    Ok(())
}
