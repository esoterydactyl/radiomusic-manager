//! Disk-level details (partition table, real FAT type) and card formatting.
//!
//! The Radio Music needs a single FAT32 (up to 32GB) or exFAT partition on an
//! MBR-partitioned card. File-system names from `sysinfo` can't tell FAT16
//! from FAT32 or see the partition table, so on macOS we ask `diskutil`.
//! Formatting is macOS-only for now.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct DiskDetails {
    /// BSD name of the whole disk, e.g. `disk7`.
    pub whole_disk: String,
    /// `MBR`, `GPT`, `APM`, `none` (no partition table) or the raw type.
    pub scheme: String,
    pub partition_count: usize,
    /// `FAT32`, `FAT16`, `FAT12`, `exFAT`, or the raw `diskutil` name.
    pub file_system: String,
    pub disk_size_bytes: u64,
    pub removable: bool,
    pub internal: bool,
}

pub const LABEL_MAX: usize = 11;
/// FAT32 is used up to this size; larger cards must be exFAT.
pub const FAT32_MAX_DISK_BYTES: u64 = 32_000_000_000;

pub fn scheme_from_content(content: &str) -> String {
    match content {
        "FDisk_partition_scheme" => "MBR".into(),
        "GUID_partition_scheme" => "GPT".into(),
        "Apple_partition_scheme" => "APM".into(),
        // A whole disk whose "content" is a file system has no partition table at all.
        c if c.starts_with("Windows_") || c.contains("FAT") || c.is_empty() => "none".into(),
        other => other.to_string(),
    }
}

pub fn classify_fs(diskutil_name: &str) -> String {
    match diskutil_name.to_ascii_lowercase().as_str() {
        "ms-dos fat32" => "FAT32".into(),
        "ms-dos fat16" => "FAT16".into(),
        "ms-dos fat12" => "FAT12".into(),
        "exfat" => "exFAT".into(),
        _ => diskutil_name.to_string(),
    }
}

/// Which file system a card of this size should be formatted with.
pub fn target_fs(disk_size_bytes: u64) -> &'static str {
    if disk_size_bytes <= FAT32_MAX_DISK_BYTES {
        "FAT32"
    } else {
        "exFAT"
    }
}

/// Why this disk must not be formatted, or `None` if it is a removable card.
pub fn format_blocker(d: &DiskDetails, boot_disk: Option<&str>) -> Option<String> {
    if boot_disk == Some(d.whole_disk.as_str()) {
        return Some("This is the disk macOS is running from".into());
    }
    if d.internal && !d.removable {
        return Some("Only removable cards can be formatted".into());
    }
    if d.disk_size_bytes == 0 {
        return Some("Disk size is unknown".into());
    }
    None
}

pub fn validate_label(label: &str) -> Result<(), String> {
    let ok_chars = label.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if label.is_empty() || label.len() > LABEL_MAX || !ok_chars {
        return Err(format!(
            "Label must be 1-{LABEL_MAX} characters: capital letters, digits, - or _"
        ));
    }
    Ok(())
}

/// `disk0s2` -> `disk0`.
pub fn whole_disk_of(id: &str) -> Option<String> {
    let digits = id.strip_prefix("disk")?.chars().take_while(char::is_ascii_digit).count();
    (digits > 0).then(|| id[..4 + digits].to_string())
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use serde_json::Value;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn plist_to_json(bytes: &[u8]) -> Result<Value, String> {
        let mut child = Command::new("plutil")
            .args(["-convert", "json", "-o", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("plutil: {e}"))?;
        child
            .stdin
            .take()
            .ok_or("plutil: no stdin")?
            .write_all(bytes)
            .map_err(|e| format!("plutil: {e}"))?;
        let out = child.wait_with_output().map_err(|e| format!("plutil: {e}"))?;
        serde_json::from_slice(&out.stdout).map_err(|e| format!("plutil output: {e}"))
    }

    fn diskutil_json(args: &[&str]) -> Result<Value, String> {
        let out = Command::new("diskutil")
            .args(args)
            .output()
            .map_err(|e| format!("diskutil: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        plist_to_json(&out.stdout)
    }

    fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
        v.get(k).and_then(Value::as_str).unwrap_or("")
    }

    fn bool_of(v: &Value, k: &str) -> bool {
        v.get(k).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn details(mount: &Path) -> Result<DiskDetails, String> {
        let mount = mount.to_string_lossy();
        let vol = diskutil_json(&["info", "-plist", &mount])?;
        let whole = if bool_of(&vol, "WholeDisk") {
            str_of(&vol, "DeviceIdentifier")
        } else {
            str_of(&vol, "ParentWholeDisk")
        }
        .to_string();
        if whole.is_empty() {
            return Err("Could not determine the whole disk".into());
        }

        let disk = diskutil_json(&["info", "-plist", &whole])?;
        let list = diskutil_json(&["list", "-plist", &whole])?;
        let partition_count = list
            .get("AllDisksAndPartitions")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|d| d.get("Partitions"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);

        Ok(DiskDetails {
            scheme: scheme_from_content(str_of(&disk, "Content")),
            partition_count,
            file_system: classify_fs(str_of(&vol, "FilesystemName")),
            disk_size_bytes: disk.get("Size").and_then(Value::as_u64).unwrap_or(0),
            removable: bool_of(&disk, "RemovableMedia") || bool_of(&disk, "Ejectable"),
            internal: bool_of(&disk, "Internal"),
            whole_disk: whole,
        })
    }

    /// Whole-disk name behind `/` (the disk macOS runs from).
    pub fn boot_disk() -> Option<String> {
        let root = diskutil_json(&["info", "-plist", "/"]).ok()?;
        // On APFS the system volume lives in a synthesized container; its physical store is the real disk.
        let store = root
            .get("APFSPhysicalStores")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|s| s.get("APFSPhysicalStore"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = store.unwrap_or_else(|| str_of(&root, "DeviceIdentifier").to_string());
        whole_disk_of(&id)
    }

    pub fn erase(whole_disk: &str, fs: &str, label: &str) -> Result<(), String> {
        let out = Command::new("diskutil")
            .args(["eraseDisk", fs, label, "MBR", &format!("/dev/{whole_disk}")])
            .output()
            .map_err(|e| format!("diskutil: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let msg = String::from_utf8_lossy(&out.stderr);
            let msg = if msg.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { msg };
            Err(format!("diskutil eraseDisk failed: {}", msg.trim()))
        }
    }
}

type Cache = Mutex<HashMap<String, (Instant, Option<DiskDetails>)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const CACHE_TTL: Duration = Duration::from_secs(30);

/// Cached disk details for a mounted volume (the volume list is polled every couple of seconds).
pub fn details(mount: &Path) -> Option<DiskDetails> {
    let key = mount.to_string_lossy().into_owned();
    if let Some((at, d)) = cache().lock().ok()?.get(&key) {
        if at.elapsed() < CACHE_TTL {
            return d.clone();
        }
    }
    #[cfg(target_os = "macos")]
    let fresh = mac::details(mount).ok();
    #[cfg(not(target_os = "macos"))]
    let fresh = None;
    cache().lock().ok()?.insert(key, (Instant::now(), fresh.clone()));
    fresh
}

pub fn invalidate_cache() {
    if let Ok(mut c) = cache().lock() {
        c.clear();
    }
}

/// Erase the whole disk behind `mount` and create one MBR partition. Returns the new mount point.
pub fn format(mount: &Path, label: &str) -> Result<String, String> {
    validate_label(label)?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = mount;
        Err("Formatting is only supported on macOS so far".into())
    }
    #[cfg(target_os = "macos")]
    {
        // Always re-read the disk fresh: never act on a cached view when erasing.
        let d = mac::details(mount)?;
        if let Some(why) = format_blocker(&d, mac::boot_disk().as_deref()) {
            return Err(format!("Refusing to format: {why}"));
        }
        let fs = target_fs(d.disk_size_bytes);
        mac::erase(&d.whole_disk, if fs == "FAT32" { "FAT32" } else { "ExFAT" }, label)?;
        invalidate_cache();
        Ok(format!("/Volumes/{label}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(internal: bool, removable: bool, whole: &str) -> DiskDetails {
        DiskDetails {
            whole_disk: whole.into(),
            scheme: "MBR".into(),
            partition_count: 1,
            file_system: "FAT32".into(),
            disk_size_bytes: 16_000_000_000,
            removable,
            internal,
        }
    }

    #[test]
    fn partition_schemes() {
        assert_eq!(scheme_from_content("FDisk_partition_scheme"), "MBR");
        assert_eq!(scheme_from_content("GUID_partition_scheme"), "GPT");
        assert_eq!(scheme_from_content("Apple_partition_scheme"), "APM");
        assert_eq!(scheme_from_content("Windows_FAT_32"), "none");
        assert_eq!(scheme_from_content(""), "none");
    }

    #[test]
    fn fat_types_are_told_apart() {
        assert_eq!(classify_fs("MS-DOS FAT32"), "FAT32");
        assert_eq!(classify_fs("MS-DOS FAT16"), "FAT16");
        assert_eq!(classify_fs("ExFAT"), "exFAT");
        assert_eq!(classify_fs("APFS"), "APFS");
    }

    #[test]
    fn target_fs_follows_card_size() {
        assert_eq!(target_fs(15_700_000_000), "FAT32");
        assert_eq!(target_fs(32_000_000_000), "FAT32");
        assert_eq!(target_fs(64_000_000_000), "exFAT");
    }

    #[test]
    fn only_removable_non_boot_disks_may_be_formatted() {
        assert!(format_blocker(&disk(false, true, "disk7"), Some("disk0")).is_none());
        assert!(format_blocker(&disk(false, false, "disk7"), Some("disk0")).is_none(), "external USB reader");
        assert!(format_blocker(&disk(true, true, "disk7"), Some("disk0")).is_none(), "built-in SD slot");
        assert!(format_blocker(&disk(true, false, "disk0"), Some("disk9")).is_some(), "internal SSD");
        assert!(format_blocker(&disk(false, true, "disk0"), Some("disk0")).is_some(), "boot disk");
        let mut zero = disk(false, true, "disk7");
        zero.disk_size_bytes = 0;
        assert!(format_blocker(&zero, None).is_some());
    }

    #[test]
    fn whole_disk_names() {
        assert_eq!(whole_disk_of("disk0s2").as_deref(), Some("disk0"));
        assert_eq!(whole_disk_of("disk12").as_deref(), Some("disk12"));
        assert_eq!(whole_disk_of("disk7s1").as_deref(), Some("disk7"));
        assert_eq!(whole_disk_of("diskX"), None);
        assert_eq!(whole_disk_of("/dev/x"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn boot_disk_is_detected_on_macos() {
        let boot = mac::boot_disk().expect("boot disk");
        assert!(boot.starts_with("disk"), "{boot}");
    }

    #[test]
    fn label_validation() {
        assert!(validate_label("RADIOMUSIC").is_ok());
        assert!(validate_label("CARD-1_A").is_ok());
        assert!(validate_label("").is_err());
        assert!(validate_label("lowercase").is_err());
        assert!(validate_label("HAS SPACE").is_err());
        assert!(validate_label("WAYTOOLONGLABEL").is_err());
        assert!(validate_label("A/B").is_err());
    }
}

#[cfg(test)]
mod real_disk {
    use super::*;

    /// Manual, DESTRUCTIVE: `RMM_TEST_VOLUME=/Volumes/X cargo test real_disk -- --ignored --nocapture`
    /// Only point this at a disposable disk image.
    #[test]
    #[ignore]
    fn inspect_and_format_real_volume() {
        let vol = std::env::var("RMM_TEST_VOLUME").expect("set RMM_TEST_VOLUME");
        let vol = Path::new(&vol);
        let before = details(vol).expect("details");
        println!("before: {before:?}");
        println!("blocker: {:?}", format_blocker(&before, None));
        let new_mount = format(vol, "RMTEST").expect("format");
        println!("formatted -> {new_mount}");
        let after = details(Path::new(&new_mount)).expect("details after");
        println!("after: {after:?}");
        assert_eq!(after.scheme, "MBR");
        assert_eq!(after.partition_count, 1);
        assert_eq!(after.file_system, "FAT32");
    }
}

#[cfg(test)]
mod probe_volumes {
    #[test]
    #[ignore]
    fn print_volumes() {
        for v in crate::volumes::list() {
            println!("{v:#?}");
        }
    }
}
