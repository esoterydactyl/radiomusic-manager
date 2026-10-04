mod audio;
mod card;
mod disk;
mod scan;
mod settings;
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
    normalize: Option<audio::NormMode>,
    capacity_bytes: Option<u64>,
    fit: bool,
) -> Result<card::Plan, String> {
    let max_file_bytes = (card_file_system.as_deref() == Some("FAT32")).then_some(card::FAT32_MAX_FILE_BYTES);
    card::plan_for_capacity(candidates, banks, files_per_bank, seed, max_file_bytes, normalize, capacity_bytes, fit)
}

/// Copies a plan onto the card at `card_path`, emitting `write-progress` events.
#[tauri::command]
async fn write_card(
    app: tauri::AppHandle,
    card_path: String,
    plan: card::Plan,
    existing: card::Existing,
) -> Result<usize, String> {
    use tauri::Emitter;
    tauri::async_runtime::spawn_blocking(move || {
        card::write(std::path::Path::new(&card_path), &plan, existing, |p| {
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

/// Describes what writing `plan` would delete, copy and leave alone, without changing the card.
#[tauri::command]
async fn preview_card_changes(
    card_path: String,
    plan: card::Plan,
    existing: card::Existing,
) -> Result<card::ChangePreview, String> {
    tauri::async_runtime::spawn_blocking(move || card::preview(std::path::Path::new(&card_path), &plan, existing))
        .await
        .map_err(|e| e.to_string())?
}

/// The Radio Music's settings: names, allowed values, defaults and help text.
#[tauri::command]
fn settings_schema() -> Vec<settings::SettingDef> {
    settings::schema()
}

/// Reads `settings.txt` from the card root (an empty result if there isn't one).
#[tauri::command]
async fn read_card_settings(card_path: String) -> Result<settings::CardSettings, String> {
    tauri::async_runtime::spawn_blocking(move || settings::read(std::path::Path::new(&card_path)))
        .await
        .map_err(|e| e.to_string())?
}

/// Saves the given settings into the card's root `settings.txt`, keeping its comments and other lines.
#[tauri::command]
async fn write_card_settings(
    card_path: String,
    values: std::collections::BTreeMap<String, i64>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || settings::write(std::path::Path::new(&card_path), &values))
        .await
        .map_err(|e| e.to_string())?
}

#[derive(serde::Serialize)]
struct AudioInfo {
    format: &'static str,
    duration_secs: f64,
    rate: u32,
    channels: u16,
    bits: u16,
    /// Left-channel min/max per bucket, for drawing a waveform.
    peaks: Vec<[f32; 2]>,
}

/// Format details and a waveform for one audio file.
#[tauri::command]
async fn audio_peaks(path: String, buckets: usize) -> Result<AudioInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pf = audio::PcmFile::open(std::path::Path::new(&path))?;
        Ok(AudioInfo {
            format: pf.format_name(),
            duration_secs: pf.duration_secs(),
            rate: pf.rate,
            channels: pf.channels,
            bits: pf.bits,
            peaks: audio::peaks(&pf, buckets.clamp(1, 8192))?,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A short mono WAV of the file's left channel (what the Radio Music plays), as raw bytes.
#[tauri::command]
async fn audio_preview(path: String, start_secs: f64, max_secs: f64) -> Result<tauri::ipc::Response, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pf = audio::PcmFile::open(std::path::Path::new(&path))?;
        audio::preview_wav(&pf, start_secs, max_secs.min(180.0)).map(tauri::ipc::Response::new)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(serde::Serialize)]
struct LevelsInfo {
    peak_db: f64,
    rms_db: f64,
    /// Gain the chosen normalize mode would apply, in dB (0 when none is chosen).
    gain_db: f64,
}

/// Peak and loudness of a file (or its trimmed part), and what normalizing would do to it.
#[tauri::command]
async fn audio_levels(path: String, trim: Option<audio::Trim>, mode: Option<audio::NormMode>) -> Result<LevelsInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pf = audio::PcmFile::open(std::path::Path::new(&path))?;
        let levels = audio::analyze(&pf, trim)?;
        let db = |v: f64| if v < 1e-9 { f64::NEG_INFINITY } else { 20.0 * v.log10() };
        let gain = mode.map_or(1.0, |m| audio::gain_for(levels, m));
        Ok(LevelsInfo { peak_db: db(levels.peak), rms_db: db(levels.rms), gain_db: db(gain) })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stops a running `write_card` after the current chunk, removing the partly written file.
#[tauri::command]
fn cancel_write() {
    card::request_cancel();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![list_volumes, scan_directory, scan_source, plan_card, write_card, preview_card_changes, cancel_write, format_card, audio_peaks, audio_preview, audio_levels, settings_schema, read_card_settings, write_card_settings])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
