//! Lists mounted volumes so the user can pick their Radio Music SD card.
//!
//! Card requirements from the reference:
//! <https://www.musicthing.co.uk/Radio_Music_Reference/>
//! SDHC up to 32GB formatted FAT32, or SDXC up to 2TB formatted exFAT.

use std::path::Path;

use serde::Serialize;
use sysinfo::Disks;

const GB: u64 = 1_000_000_000;
const MAX_FAT32_BYTES: u64 = 32 * GB;
const MAX_EXFAT_BYTES: u64 = 2_000 * GB;

#[derive(Serialize, Debug)]
pub struct Volume {
    pub name: String,
    pub mount_point: String,
    /// Normalised: `FAT32`, `exFAT`, or the raw name reported by the OS.
    pub file_system: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub removable: bool,
    /// Why this volume wouldn't work as a Radio Music card (empty if it looks fine).
    pub warnings: Vec<String>,
}

fn normalise_fs(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "msdos" | "vfat" | "fat" | "fat32" => "FAT32".into(),
        "exfat" => "exFAT".into(),
        _ => raw.to_string(),
    }
}

fn warnings_for(file_system: &str, total_bytes: u64) -> Vec<String> {
    let mut w = Vec::new();
    match file_system {
        "FAT32" if total_bytes > MAX_FAT32_BYTES => w.push(
            "FAT32 cards are supported up to 32GB; larger cards must be exFAT".to_string(),
        ),
        "FAT32" => {}
        "exFAT" if total_bytes > MAX_EXFAT_BYTES => {
            w.push("Cards above 2TB are not supported".to_string())
        }
        "exFAT" => {}
        other => w.push(format!(
            "{} is not supported; the card must be FAT32 or exFAT",
            if other.is_empty() { "Unknown filesystem" } else { other }
        )),
    }
    w
}

/// System volumes we never want to offer as a card.
fn is_system_mount(mount: &Path) -> bool {
    let m = mount.to_string_lossy();
    m == "/"
        || m == "/boot"
        || m.starts_with("/boot/")
        || m.starts_with("/System")
        || m.starts_with("/private")
        || m.starts_with("/snap")
        || m.starts_with("/var")
        || m.starts_with("/proc")
        || m.starts_with("/dev")
}

pub fn list() -> Vec<Volume> {
    let disks = Disks::new_with_refreshed_list();
    let mut volumes: Vec<Volume> = disks
        .list()
        .iter()
        .filter(|d| !is_system_mount(d.mount_point()))
        .map(|d| {
            let file_system = normalise_fs(&d.file_system().to_string_lossy());
            let total_bytes = d.total_space();
            Volume {
                name: d.name().to_string_lossy().into_owned(),
                mount_point: d.mount_point().to_string_lossy().into_owned(),
                warnings: warnings_for(&file_system, total_bytes),
                file_system,
                total_bytes,
                available_bytes: d.available_space(),
                removable: d.is_removable(),
            }
        })
        .collect();
    volumes.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    volumes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_names_are_normalised() {
        assert_eq!(normalise_fs("msdos"), "FAT32");
        assert_eq!(normalise_fs("vfat"), "FAT32");
        assert_eq!(normalise_fs("exfat"), "exFAT");
        assert_eq!(normalise_fs("apfs"), "apfs");
    }

    #[test]
    fn card_warnings() {
        assert!(warnings_for("FAT32", 16 * GB).is_empty());
        assert!(warnings_for("exFAT", 256 * GB).is_empty());
        assert_eq!(warnings_for("FAT32", 64 * GB).len(), 1);
        assert_eq!(warnings_for("exFAT", 3_000 * GB).len(), 1);
        assert_eq!(warnings_for("ntfs", 8 * GB).len(), 1);
        assert_eq!(warnings_for("", 8 * GB).len(), 1);
    }

    #[test]
    fn system_mounts_are_excluded() {
        assert!(is_system_mount(Path::new("/")));
        assert!(is_system_mount(Path::new("/System/Volumes/Data")));
        assert!(!is_system_mount(Path::new("/Volumes/SDCARD")));
        assert!(!is_system_mount(Path::new("/media/user/CARD")));
    }
}
