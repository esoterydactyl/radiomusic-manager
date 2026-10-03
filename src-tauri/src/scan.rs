//! Scans a directory (typically a mounted SD card) for RadioMusic audio files.
//!
//! Rules follow the Music Thing Modular Radio Music reference:
//! <https://www.musicthing.co.uk/Radio_Music_Reference/>

use std::path::{Path, PathBuf};

use lofty::file::AudioFile;
use lofty::probe::Probe;
use serde::Serialize;
use walkdir::WalkDir;

const MAX_BANKS: usize = 16;
const MAX_FILES_PER_BANK: usize = 250;
const MAX_FILES_TOTAL: usize = 800;
const MAX_FOLDERS: usize = 64;
const MAX_PATH_CHARS: usize = 80_000;
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 96_000;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Wav,
    Aiff,
    Raw,
}

impl Format {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "wav" => Some(Self::Wav),
            "aif" | "aiff" => Some(Self::Aiff),
            "raw" => Some(Self::Raw),
            _ => None,
        }
    }
}

#[derive(Serialize, Debug)]
pub struct AudioFileInfo {
    pub path: String,
    /// Path relative to the scanned root, with `/` separators.
    pub relative_path: String,
    /// Bank number 0-15 from the top-level folder name; `None` for root files.
    pub bank: Option<u8>,
    pub format: Format,
    pub size_bytes: u64,
    pub duration_secs: Option<f64>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    /// Problems that would stop the RadioMusic playing this file correctly.
    pub warnings: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct ScanResult {
    pub root: String,
    pub files: Vec<AudioFileInfo>,
    pub folder_count: usize,
    /// Card-level problems (limits exceeded, etc.).
    pub warnings: Vec<String>,
}

/// Leading digits of a folder name (`"05 shortwave"` -> 5), if a valid bank 0-15.
fn bank_from_folder(name: &str) -> Option<u8> {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: u8 = digits.parse().ok()?;
    (usize::from(n) < MAX_BANKS).then_some(n)
}

/// Files and folders starting with `.` or `_` are ignored by the firmware.
pub(crate) fn is_hidden(name: &str) -> bool {
    name.starts_with('.') || name.starts_with('_')
}

pub(crate) fn read_file(root: &Path, path: &Path, format: Format, bank: Option<u8>) -> AudioFileInfo {
    let relative_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");

    let mut info = AudioFileInfo {
        path: path.to_string_lossy().into_owned(),
        relative_path,
        bank,
        format,
        size_bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        duration_secs: None,
        sample_rate: None,
        bit_depth: None,
        channels: None,
        warnings: Vec::new(),
    };

    if format == Format::Raw {
        // Headerless: the firmware assumes 44.1kHz, 16-bit mono.
        info.sample_rate = Some(44_100);
        info.bit_depth = Some(16);
        info.channels = Some(1);
        info.duration_secs = Some(info.size_bytes as f64 / (44_100.0 * 2.0));
        return info;
    }

    match Probe::open(path).and_then(|p| p.read()) {
        Ok(tagged) => {
            let props = tagged.properties();
            info.duration_secs = Some(props.duration().as_secs_f64());
            info.sample_rate = props.sample_rate();
            info.bit_depth = props.bit_depth();
            info.channels = props.channels();

        }
        Err(e) => info.warnings.push(format!("Could not read file: {e}")),
    }

    if let Some(rate) = info.sample_rate {
        if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&rate) {
            info.warnings.push(format!(
                "Sample rate {rate} Hz is outside the supported {MIN_SAMPLE_RATE}-{MAX_SAMPLE_RATE} Hz range"
            ));
        }
    }
    if let Some(ch) = info.channels {
        if ch > 2 {
            info.warnings
                .push(format!("{ch} channels; only mono or stereo files are supported"));
        }
    }
    if let Some(depth) = info.bit_depth {
        if !matches!(depth, 8 | 16 | 24) {
            info.warnings
                .push(format!("Bit depth {depth} is not supported (use 8, 16 or 24)"));
        }
    }

    info
}

pub fn scan(root: &Path) -> Result<ScanResult, String> {
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }

    let mut files: Vec<AudioFileInfo> = Vec::new();
    let mut folder_count = 0usize;
    let mut path_chars = 0usize;
    let mut warnings = Vec::new();

    let walker = WalkDir::new(root)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !is_hidden(&e.file_name().to_string_lossy()));

    for entry in walker {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();

        // The first path component below root decides the bank.
        let top: PathBuf = path
            .strip_prefix(root)
            .ok()
            .and_then(|p| p.components().next())
            .map(|c| c.as_os_str().into())
            .unwrap_or_default();
        let top_is_folder = entry.depth() > 1 || entry.file_type().is_dir();
        let bank = if top_is_folder {
            bank_from_folder(&top.to_string_lossy())
        } else {
            None
        };

        if entry.file_type().is_dir() {
            folder_count += 1;
            continue;
        }

        // Unnumbered root folders are ignored by the firmware.
        if top_is_folder && bank.is_none() {
            continue;
        }

        let Some(format) = Format::from_path(path) else {
            continue;
        };

        path_chars += path.to_string_lossy().chars().count();
        files.push(read_file(root, path, format, bank));
    }

    if folder_count > MAX_FOLDERS {
        warnings.push(format!("{folder_count} folders found; the limit is {MAX_FOLDERS}"));
    }
    if files.len() > MAX_FILES_TOTAL {
        warnings.push(format!(
            "{} audio files found; the limit is {MAX_FILES_TOTAL} per card",
            files.len()
        ));
    }
    if path_chars > MAX_PATH_CHARS {
        warnings.push(format!(
            "Combined path length is {path_chars} characters; the limit is {MAX_PATH_CHARS}"
        ));
    }
    let mut per_bank = std::collections::BTreeMap::<Option<u8>, usize>::new();
    for f in &files {
        *per_bank.entry(f.bank).or_default() += 1;
    }
    for (bank, count) in per_bank {
        if count > MAX_FILES_PER_BANK {
            let label = bank.map_or("root".to_string(), |b| format!("bank {b}"));
            warnings.push(format!(
                "{label} has {count} audio files; the limit is {MAX_FILES_PER_BANK}"
            ));
        }
    }

    Ok(ScanResult {
        root: root.to_string_lossy().into_owned(),
        files,
        folder_count,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank_parsing() {
        assert_eq!(bank_from_folder("05 shortwave"), Some(5));
        assert_eq!(bank_from_folder("15 - Koto plucks"), Some(15));
        assert_eq!(bank_from_folder("16 nope"), None);
        assert_eq!(bank_from_folder("drums"), None);
    }

    #[test]
    fn format_detection() {
        assert_eq!(Format::from_path(Path::new("a/B.WAV")), Some(Format::Wav));
        assert_eq!(Format::from_path(Path::new("a.aif")), Some(Format::Aiff));
        assert_eq!(Format::from_path(Path::new("a.mp3")), None);
    }

    #[test]
    fn scan_respects_banks_and_ignores() {
        let dir = std::env::temp_dir().join(format!("rmm-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["00 a", "unnumbered", ".hidden"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in ["root.raw", "00 a/x.raw", "unnumbered/y.raw", ".hidden/z.raw", "_skip.raw", "n.txt"] {
            std::fs::write(dir.join(f), [0u8; 88_200]).unwrap();
        }

        let result = scan(&dir).unwrap();
        let mut got: Vec<_> = result
            .files
            .iter()
            .map(|f| (f.relative_path.as_str(), f.bank))
            .collect();
        got.sort();
        assert_eq!(got, vec![("00 a/x.raw", Some(0)), ("root.raw", None)]);
        assert_eq!(result.files[0].duration_secs, Some(1.0));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
