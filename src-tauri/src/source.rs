//! Scans a user-supplied source folder for samples that can go on a Radio Music card.
//!
//! Unlike a card scan there is no bank structure here: every audio file in the
//! tree is a candidate, and is eligible if the Radio Music can play it.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use walkdir::WalkDir;

use crate::scan::{is_hidden, read_file, AudioFileInfo, Format};

#[derive(Serialize, Debug)]
pub struct SourceFile {
    #[serde(flatten)]
    pub info: AudioFileInfo,
    /// True when the file is a supported format with no warnings.
    pub eligible: bool,
}

#[derive(Serialize, Debug)]
pub struct SourceScan {
    pub root: String,
    pub files: Vec<SourceFile>,
    pub eligible_count: usize,
    /// Audio-looking files the Radio Music can't play, keyed by extension (e.g. `mp3`: 12).
    pub unsupported_by_extension: BTreeMap<String, usize>,
}

/// Extensions we recognise as audio but the Radio Music cannot play.
const UNSUPPORTED_AUDIO: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "aac", "wma", "alac"];

pub fn scan(root: &Path) -> Result<SourceScan, String> {
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }

    let mut files = Vec::new();
    let mut unsupported_by_extension = BTreeMap::new();

    let walker = WalkDir::new(root)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !is_hidden(&e.file_name().to_string_lossy()));

    for entry in walker {
        // Unreadable entries (permissions, broken links) shouldn't abort a whole library scan.
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();

        if let Some(format) = Format::from_path(path) {
            let info = read_file(root, path, format, None);
            let eligible = info.warnings.is_empty();
            files.push(SourceFile { info, eligible });
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext = ext.to_ascii_lowercase();
            if UNSUPPORTED_AUDIO.contains(&ext.as_str()) {
                *unsupported_by_extension.entry(ext).or_default() += 1;
            }
        }
    }

    let eligible_count = files.iter().filter(|f| f.eligible).count();
    Ok(SourceScan {
        root: root.to_string_lossy().into_owned(),
        files,
        eligible_count,
        unsupported_by_extension,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_unsupported_and_skips_hidden() {
        let dir = std::env::temp_dir().join(format!("rmm-source-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join(".trash")).unwrap();
        for f in ["a.raw", "sub/b.RAW", "x.mp3", "sub/y.MP3", "z.flac", "notes.txt", ".trash/c.raw", "_d.raw"] {
            std::fs::write(dir.join(f), [0u8; 4_410]).unwrap();
        }

        let result = scan(&dir).unwrap();
        let mut names: Vec<_> = result.files.iter().map(|f| f.info.relative_path.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a.raw", "sub/b.RAW"]);
        assert_eq!(result.eligible_count, 2);
        assert_eq!(result.unsupported_by_extension.get("mp3"), Some(&2));
        assert_eq!(result.unsupported_by_extension.get("flac"), Some(&1));
        assert!(!result.unsupported_by_extension.contains_key("txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_wav_is_ineligible() {
        let dir = std::env::temp_dir().join(format!("rmm-source-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.wav"), b"not a wav").unwrap();

        let result = scan(&dir).unwrap();
        assert_eq!(result.files.len(), 1);
        assert!(!result.files[0].eligible);
        assert_eq!(result.eligible_count, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
