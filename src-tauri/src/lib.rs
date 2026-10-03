mod scan;
mod source;
mod volumes;

/// Lists mounted volumes (excluding system volumes) with card-suitability warnings.
#[tauri::command]
async fn list_volumes() -> Result<Vec<volumes::Volume>, String> {
    tauri::async_runtime::spawn_blocking(volumes::list)
        .await
        .map_err(|e| e.to_string())
}

/// Scans `path` for RadioMusic-compatible audio files and reads their metadata.
#[tauri::command]
async fn scan_directory(path: String) -> Result<scan::ScanResult, String> {
    tauri::async_runtime::spawn_blocking(move || scan::scan(std::path::Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
}

/// Scans a source folder (any directory of audio) and reports which files are
/// eligible for a Radio Music card.
#[tauri::command]
async fn scan_source(path: String) -> Result<source::SourceScan, String> {
    tauri::async_runtime::spawn_blocking(move || source::scan(std::path::Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![list_volumes, scan_directory, scan_source])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
