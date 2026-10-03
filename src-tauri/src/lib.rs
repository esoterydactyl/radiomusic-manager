mod card;
mod disk;
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

/// Randomly chooses files from `candidates` and lays them out across `banks` banks.
#[tauri::command]
fn plan_card(
    candidates: Vec<card::Candidate>,
    banks: usize,
    files_per_bank: usize,
    seed: u64,
    card_file_system: Option<String>,
) -> Result<card::Plan, String> {
    let max_file_bytes = (card_file_system.as_deref() == Some("FAT32")).then_some(card::FAT32_MAX_FILE_BYTES);
    card::plan(candidates, banks, files_per_bank, seed, max_file_bytes)
}

/// Copies a plan onto the card at `card_path`, emitting `write-progress` events.
#[tauri::command]
async fn write_card(app: tauri::AppHandle, card_path: String, plan: card::Plan) -> Result<usize, String> {
    use tauri::Emitter;
    tauri::async_runtime::spawn_blocking(move || {
        card::write(std::path::Path::new(&card_path), &plan, |p| {
            let _ = app.emit("write-progress", p);
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Erases the whole disk behind `mount_point` and creates one MBR partition. Destructive.
#[tauri::command]
async fn format_card(mount_point: String, label: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Only ever format a volume we are currently listing as a card candidate.
        let listed = volumes::list().into_iter().find(|v| v.mount_point == mount_point);
        match listed {
            None => Err(format!("{mount_point} is not a mounted volume")),
            Some(v) if !v.formattable => Err(v.format_blocker.unwrap_or_else(|| "This volume cannot be formatted".into())),
            Some(_) => disk::format(std::path::Path::new(&mount_point), &label),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![list_volumes, scan_directory, scan_source, plan_card, write_card, format_card])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
