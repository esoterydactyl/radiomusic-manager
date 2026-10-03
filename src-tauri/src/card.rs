//! Plans and writes a randomized Radio Music card.
//!
//! Layout rules (see <https://www.musicthing.co.uk/Radio_Music_Reference/>):
//! one bank is just the card root; several banks are folders named `00`..`15`.
//! Limits: 16 banks, 250 files per bank, 800 files per card. Files whose names
//! start with `.` or `_` are ignored by the firmware, so we never produce them.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::path::{Component, Path, PathBuf};

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};

use crate::{scan, volumes};

pub const MAX_BANKS: usize = 16;
pub const MAX_FILES_PER_BANK: usize = 250;
pub const MAX_FILES_TOTAL: usize = 800;
/// FAT32 cannot hold a single file of 4 GiB or more.
pub const FAT32_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024 - 1;
const MAX_NAME_CHARS: usize = 100;

#[derive(Deserialize, Debug, Clone)]
pub struct Candidate {
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PlannedFile {
    pub src: String,
    /// Destination relative to the card root, `/`-separated (e.g. `03/kick.wav`).
    pub dest: String,
    pub size_bytes: u64,
    /// Bank number 0-15, or `None` when files go in the card root.
    pub bank: Option<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Plan {
    pub seed: u64,
    pub banks: usize,
    pub files: Vec<PlannedFile>,
    pub total_bytes: u64,
    pub warnings: Vec<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Progress {
    /// `deleting`, then `writing` while copying, then `flushing` for the final flush to the card.
    pub phase: &'static str,
    pub done_files: usize,
    pub total_files: usize,
    /// Destination (relative to the card) of the file being copied.
    pub current: String,
    /// Bank of the current file (`None` when files go in the card root).
    pub bank: Option<u8>,
    pub bank_done: usize,
    pub bank_total: usize,
    /// Bytes copied so far, including the part of the current file already written.
    pub bytes_done: u64,
    pub bytes_total: u64,
}

static CANCEL: AtomicBool = AtomicBool::new(false);

/// Ask a running `write` to stop after the current chunk.
pub fn request_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

const COPY_CHUNK: usize = 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Replace characters that FAT/exFAT forbid. Uses `-` rather than `_` so the
/// result can never start with the `_` the firmware treats as "ignore me".
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim_matches(|c: char| c == ' ' || c == '.' || c == '_');
    let (stem, ext) = match cleaned.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (cleaned, String::new()),
    };
    let stem: String = stem.chars().take(MAX_NAME_CHARS.saturating_sub(ext.chars().count())).collect();
    let stem = stem.trim_end();
    if stem.is_empty() {
        format!("sample{ext}")
    } else {
        format!("{stem}{ext}")
    }
}

/// Give `name` a unique spelling within `taken` (case-insensitive, as FAT is).
fn unique_name(name: String, taken: &mut HashSet<String>) -> String {
    if taken.insert(name.to_lowercase()) {
        return name;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.clone(), String::new()),
    };
    for n in 2.. {
        let candidate = format!("{stem} ({n}){ext}");
        if taken.insert(candidate.to_lowercase()) {
            return candidate;
        }
    }
    unreachable!()
}

fn bank_dir(banks: usize, bank: usize) -> String {
    if banks == 1 {
        String::new()
    } else {
        format!("{bank:02}")
    }
}

/// Randomly choose and distribute files across `banks` banks.
pub fn plan(
    candidates: Vec<Candidate>,
    banks: usize,
    files_per_bank: usize,
    seed: u64,
    max_file_bytes: Option<u64>,
) -> Result<Plan, String> {
    if !(1..=MAX_BANKS).contains(&banks) {
        return Err(format!("Banks must be between 1 and {MAX_BANKS}"));
    }
    if files_per_bank == 0 {
        return Err("Files per bank must be at least 1".into());
    }

    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut pool: Vec<Candidate> = candidates.into_iter().filter(|c| seen.insert(c.path.clone())).collect();

    if let Some(max) = max_file_bytes {
        let before = pool.len();
        pool.retain(|c| c.size_bytes <= max);
        if pool.len() < before {
            warnings.push(format!(
                "{} file(s) over the card's maximum file size were left out",
                before - pool.len()
            ));
        }
    }

    let per_bank = files_per_bank.min(MAX_FILES_PER_BANK);
    if per_bank < files_per_bank {
        warnings.push(format!("Files per bank capped at {MAX_FILES_PER_BANK}"));
    }
    let wanted = (banks * per_bank).min(MAX_FILES_TOTAL);
    if banks * per_bank > MAX_FILES_TOTAL {
        warnings.push(format!("Total capped at {MAX_FILES_TOTAL} files per card"));
    }
    if pool.len() < wanted {
        warnings.push(format!(
            "Only {} eligible file(s) available for {wanted} requested slots",
            pool.len()
        ));
    }

    // Sort first so the same seed always gives the same result regardless of input order.
    pool.sort_by(|a, b| a.path.cmp(&b.path));
    let mut rng = StdRng::seed_from_u64(seed);
    pool.shuffle(&mut rng);
    pool.truncate(wanted);

    // Deal round-robin so banks end up within one file of each other.
    let mut taken: Vec<HashSet<String>> = vec![HashSet::new(); banks];
    let mut files = Vec::with_capacity(pool.len());
    for (i, c) in pool.into_iter().enumerate() {
        let bank = i % banks;
        let original = Path::new(&c.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = unique_name(sanitize_name(&original), &mut taken[bank]);
        let dir = bank_dir(banks, bank);
        files.push(PlannedFile {
            src: c.path,
            dest: if dir.is_empty() { name } else { format!("{dir}/{name}") },
            size_bytes: c.size_bytes,
            bank: (banks > 1).then_some(bank as u8),
        });
    }
    // Present bank by bank, each in the name order the firmware will use.
    files.sort_by(|a, b| (a.bank, a.dest.to_lowercase()).cmp(&(b.bank, b.dest.to_lowercase())));

    let total_bytes = files.iter().map(|f| f.size_bytes).sum();
    Ok(Plan { seed, banks, files, total_bytes, warnings })
}

/// Destination must be a plain relative path with no way out of the card root.
fn safe_dest(card: &Path, dest: &str) -> Result<PathBuf, String> {
    let rel = Path::new(dest);
    if dest.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!("Refusing unsafe destination path: {dest}"));
    }
    Ok(card.join(rel))
}

fn validate(plan: &Plan) -> Result<(), String> {
    if !(1..=MAX_BANKS).contains(&plan.banks) {
        return Err("Plan has an invalid bank count".into());
    }
    if plan.files.len() > MAX_FILES_TOTAL {
        return Err(format!("Plan has more than {MAX_FILES_TOTAL} files"));
    }
    let mut per_bank = std::collections::HashMap::<Option<u8>, usize>::new();
    let mut dests = HashSet::new();
    for f in &plan.files {
        *per_bank.entry(f.bank).or_default() += 1;
        if !dests.insert(f.dest.to_lowercase()) {
            return Err(format!("Plan has a duplicate destination: {}", f.dest));
        }
        let name = f.dest.rsplit('/').next().unwrap_or("");
        if name.starts_with('.') || name.starts_with('_') {
            return Err(format!("{name} would be ignored by the Radio Music"));
        }
    }
    if per_bank.values().any(|&n| n > MAX_FILES_PER_BANK) {
        return Err(format!("A bank has more than {MAX_FILES_PER_BANK} files"));
    }
    Ok(())
}

/// Copy `src` to `dest` in large chunks, calling `on_chunk(bytes_so_far)` after each. A `false`
/// return stops the copy: the partial file is removed and `Interrupted` is returned.
///
/// There is deliberately no per-file flush here. A full flush (`F_FULLFSYNC` on macOS) makes the
/// card flush its internal cache and costs seconds per file on removable flash; the caller flushes
/// once at the end instead.
fn copy_file(src: &Path, dest: &Path, mut on_chunk: impl FnMut(u64) -> bool) -> std::io::Result<()> {
    let mut reader = File::open(src)?;
    let mut writer = File::create(dest)?;
    let mut buf = vec![0u8; COPY_CHUNK];
    let mut copied = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        copied += n as u64;
        if !on_chunk(copied) {
            drop(writer);
            let _ = std::fs::remove_file(dest);
            return Err(std::io::ErrorKind::Interrupted.into());
        }
    }
    writer.flush()
}

/// Push everything written so far out to the card. Called once, after the last file.
fn flush_to_card(last_file: &Path) {
    // Flush the OS write cache for all volumes, then force the card itself to commit.
    #[cfg(unix)]
    let _ = std::process::Command::new("sync").status();
    if let Ok(f) = File::open(last_file) {
        let _ = f.sync_all();
    }
}

/// What to do about audio already on the card.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Existing {
    /// Stop if the card already has audio (nothing is ever deleted).
    Refuse,
    /// Delete all existing audio first, then copy everything.
    Replace,
    /// Keep existing audio and add the selection alongside it.
    Add,
}

#[derive(Serialize, Debug, Clone)]
pub struct ChangePreview {
    pub delete_files: usize,
    pub delete_bytes: u64,
    pub copy_files: usize,
    pub copy_bytes: u64,
    /// Planned files already on the card with the same name and size (`Add` mode), so not copied.
    pub unchanged_files: usize,
    /// Things worth knowing before confirming; none of them block the write.
    pub warnings: Vec<String>,
}

struct Changes {
    /// Existing audio to delete, as paths relative to the card root.
    delete: Vec<(String, u64)>,
    /// Files to copy, with their final destinations (renamed if they would clash with existing audio).
    copies: Vec<PlannedFile>,
    unchanged: usize,
    warnings: Vec<String>,
}

fn bank_of(rel: &str) -> Option<u8> {
    let (top, _) = rel.split_once('/')?;
    scan::bank_from_folder(top)
}

fn bank_name(bank: Option<u8>) -> String {
    bank.map_or("root".to_string(), |b| format!("bank {b:02}"))
}

/// Work out what `mode` means for `plan` given what is on `card` right now.
fn compute_changes(card: &Path, plan: &Plan, mode: Existing) -> Result<Changes, String> {
    // The scan sees exactly the audio the Radio Music would read; nothing else is ever a candidate.
    let on_card: Vec<(String, u64)> = scan::scan(card)?
        .files
        .into_iter()
        .map(|f| (f.relative_path, f.size_bytes))
        .collect();

    match mode {
        Existing::Refuse if !on_card.is_empty() => Err(format!(
            "The card already has {} audio file(s). Choose \"Erase Existing Audio\" or \"Add new files\" to go ahead.",
            on_card.len()
        )),
        Existing::Refuse => Ok(Changes { delete: Vec::new(), copies: plan.files.clone(), unchanged: 0, warnings: Vec::new() }),
        Existing::Replace => Ok(Changes { delete: on_card, copies: plan.files.clone(), unchanged: 0, warnings: Vec::new() }),
        Existing::Add => {
            let existing_sizes: std::collections::HashMap<String, u64> =
                on_card.iter().map(|(r, s)| (r.to_lowercase(), *s)).collect();
            let mut taken: HashSet<String> = existing_sizes.keys().cloned().collect();
            let mut copies = Vec::new();
            let mut unchanged = 0;

            for f in &plan.files {
                let (dir, name) = f.dest.rsplit_once('/').map_or(("", f.dest.as_str()), |(d, n)| (d, n));
                let (stem, ext) = name.rsplit_once('.').map_or((name, String::new()), |(s, e)| (s, format!(".{e}")));
                let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
                let renamed = |n: u32| format!("{prefix}{stem} ({n}){ext}");

                let clash = existing_sizes.get(&f.dest.to_lowercase());
                match clash {
                    // Already there: nothing to do.
                    Some(&size) if size == f.size_bytes => unchanged += 1,
                    // Same name but a different file. If an earlier "Add" already stored this one
                    // under a " (n)" name, it is already there too; otherwise keep both by renaming.
                    Some(_) => {
                        let already = (2..=99).any(|n| existing_sizes.get(&renamed(n).to_lowercase()) == Some(&f.size_bytes));
                        if already {
                            unchanged += 1;
                        } else {
                            let mut file = f.clone();
                            file.dest = (2..)
                                .map(renamed)
                                .find(|c| !taken.contains(&c.to_lowercase()))
                                .expect("unbounded range");
                            taken.insert(file.dest.to_lowercase());
                            copies.push(file);
                        }
                    }
                    None => {
                        taken.insert(f.dest.to_lowercase());
                        copies.push(f.clone());
                    }
                }
            }

            // Existing audio counts toward the card's limits.
            let mut per_bank = std::collections::HashMap::<Option<u8>, usize>::new();
            for (rel, _) in &on_card {
                *per_bank.entry(bank_of(rel)).or_default() += 1;
            }
            for f in &copies {
                *per_bank.entry(f.bank).or_default() += 1;
            }
            if let Some((bank, n)) = per_bank.iter().find(|(_, &n)| n > MAX_FILES_PER_BANK) {
                return Err(format!(
                    "Adding these would put {} at {n} files; the limit is {MAX_FILES_PER_BANK}. Add fewer files or erase first.",
                    bank_name(*bank)
                ));
            }
            let total = on_card.len() + copies.len();
            if total > MAX_FILES_TOTAL {
                return Err(format!(
                    "Adding these would put the card at {total} files; the limit is {MAX_FILES_TOTAL}. Add fewer files or erase first."
                ));
            }

            let mut warnings = Vec::new();
            let has_root = on_card.iter().any(|(r, _)| bank_of(r).is_none());
            let has_banks = on_card.iter().any(|(r, _)| bank_of(r).is_some());
            let adds_root = copies.iter().any(|f| f.bank.is_none());
            let adds_banks = copies.iter().any(|f| f.bank.is_some());
            if (has_root && adds_banks) || (has_banks && adds_root) {
                warnings.push(
                    "The card's existing layout differs from the new one (root files vs bank folders), \
                     so the Radio Music may not use all of the files."
                        .to_string(),
                );
            }
            Ok(Changes { delete: Vec::new(), copies, unchanged, warnings })
        }
    }
}

fn preview_from(changes: &Changes) -> ChangePreview {
    ChangePreview {
        delete_files: changes.delete.len(),
        delete_bytes: changes.delete.iter().map(|(_, s)| s).sum(),
        copy_files: changes.copies.len(),
        copy_bytes: changes.copies.iter().map(|f| f.size_bytes).sum(),
        unchanged_files: changes.unchanged,
        warnings: changes.warnings.clone(),
    }
}

/// Describe what writing `plan` to `card` in `mode` would do, without changing anything.
pub fn preview(card: &Path, plan: &Plan, mode: Existing) -> Result<ChangePreview, String> {
    validate(plan)?;
    if !card.is_dir() {
        return Err(format!("{} is not a directory", card.display()));
    }
    Ok(preview_from(&compute_changes(card, plan, mode)?))
}

/// macOS stores a file's extended attributes on FAT as a hidden `._name` stub next to it. The Radio
/// Music ignores these, but they clutter the card and stop empty bank folders being tidied up.
fn remove_appledouble(file: &Path) {
    if let Some(name) = file.file_name() {
        let stub = file.with_file_name(format!("._{}", name.to_string_lossy()));
        let _ = std::fs::remove_file(stub);
    }
}

/// Delete `files` (card-relative), then remove bank folders they leave empty.
fn delete_audio(card: &Path, files: &[(String, u64)], mut on_each: impl FnMut(usize, &str)) -> Result<(), String> {
    for (i, (rel, _)) in files.iter().enumerate() {
        let path = safe_dest(card, rel)?;
        on_each(i, rel);
        std::fs::remove_file(&path).map_err(|e| format!("Could not delete {rel}: {e}"))?;
        remove_appledouble(&path);
        // Walk up, removing folders that are now empty. Only numbered bank folders (and anything
        // inside them) are ever pruned, and the card root never is.
        let mut dir = path.parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            if d == card {
                break;
            }
            let top = d.strip_prefix(card).ok().and_then(|r| r.components().next());
            let is_bank = top.is_some_and(|c| scan::bank_from_folder(&c.as_os_str().to_string_lossy()).is_some());
            let empty = std::fs::read_dir(&d).map(|mut r| r.next().is_none()).unwrap_or(false);
            if !is_bank || !empty || std::fs::remove_dir(&d).is_err() {
                break;
            }
            remove_appledouble(&d);
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    Ok(())
}

/// Copy the plan onto `card`, handling audio already there according to `mode`.
pub fn write(card: &Path, plan: &Plan, mode: Existing, mut on_progress: impl FnMut(Progress)) -> Result<usize, String> {
    validate(plan)?;
    if !card.is_dir() {
        return Err(format!("{} is not a directory", card.display()));
    }

    let changes = compute_changes(card, plan, mode)?;
    let summary = preview_from(&changes);

    let volume = volumes::list()
        .into_iter()
        .find(|v| Path::new(&v.mount_point) == card)
        .ok_or("That folder is not a mounted volume's root")?;
    // Deleting frees space before the copy starts, so count it.
    let available = volume.available_bytes + summary.delete_bytes;
    if summary.copy_bytes > available {
        return Err(format!(
            "Not enough space: need {} MB, card will have {} MB free",
            summary.copy_bytes / 1_000_000,
            available / 1_000_000
        ));
    }

    CANCEL.store(false, Ordering::SeqCst);

    if !changes.delete.is_empty() {
        let total = changes.delete.len();
        delete_audio(card, &changes.delete, |i, rel| {
            on_progress(Progress {
                phase: "deleting",
                done_files: i,
                total_files: total,
                current: rel.to_string(),
                bank: None,
                bank_done: 0,
                bank_total: 0,
                bytes_done: 0,
                bytes_total: 0,
            });
        })?;
    }

    let to_copy: Vec<&PlannedFile> = changes.copies.iter().collect();
    let total = to_copy.len();
    let bytes_total = summary.copy_bytes;
    let mut bank_totals = std::collections::HashMap::<Option<u8>, usize>::new();
    for f in &to_copy {
        *bank_totals.entry(f.bank).or_default() += 1;
    }
    let mut bank_done = std::collections::HashMap::<Option<u8>, usize>::new();
    let mut bytes_before = 0u64;
    let mut last_dest: Option<PathBuf> = None;

    for (i, f) in to_copy.iter().enumerate() {
        if CANCEL.load(Ordering::SeqCst) {
            return Err(format!("Cancelled after {i} of {total} files"));
        }
        let dest = safe_dest(card, &f.dest)?;
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
            remove_appledouble(dir);
        }

        let base = bytes_before;
        let snapshot = |bytes_in_file: u64, done_files: usize, bank_done_now: usize| Progress {
            phase: "writing",
            done_files,
            total_files: total,
            current: f.dest.clone(),
            bank: f.bank,
            bank_done: bank_done_now,
            bank_total: bank_totals[&f.bank],
            bytes_done: base + bytes_in_file,
            bytes_total,
        };
        let done_in_bank = *bank_done.get(&f.bank).unwrap_or(&0);
        on_progress(snapshot(0, i, done_in_bank));

        let mut last_report = Instant::now();
        let result = copy_file(Path::new(&f.src), &dest, |copied| {
            if CANCEL.load(Ordering::SeqCst) {
                return false;
            }
            if last_report.elapsed() >= PROGRESS_INTERVAL {
                last_report = Instant::now();
                on_progress(snapshot(copied, i, done_in_bank));
            }
            true
        });
        match result {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                return Err(format!("Cancelled after {i} of {total} files"));
            }
            Err(e) => return Err(format!("Copying {} failed after {i} of {total} files: {e}", f.src)),
        }

        remove_appledouble(&dest);
        bytes_before += f.size_bytes;
        *bank_done.entry(f.bank).or_default() += 1;
        on_progress(snapshot(f.size_bytes, i + 1, done_in_bank + 1));
        last_dest = Some(dest);
    }

    if last_dest.is_some() || !changes.delete.is_empty() {
        on_progress(Progress {
            phase: "flushing",
            done_files: total,
            total_files: total,
            current: String::new(),
            bank: None,
            bank_done: 0,
            bank_total: 0,
            bytes_done: bytes_total,
            bytes_total,
        });
        flush_to_card(last_dest.as_deref().unwrap_or(card));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(n: usize) -> Vec<Candidate> {
        (0..n)
            .map(|i| Candidate { path: format!("/lib/f{i:03}.wav"), size_bytes: 1_000 })
            .collect()
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_name("a:b*c.wav"), "a-b-c.wav");
        assert_eq!(sanitize_name("_hidden.wav"), "hidden.wav");
        assert_eq!(sanitize_name("  spaced.wav "), "spaced.wav");
        assert_eq!(sanitize_name("???"), "---");
        assert_eq!(sanitize_name("..."), "sample");
        assert!(sanitize_name(&format!("{}.wav", "x".repeat(300))).chars().count() <= MAX_NAME_CHARS);
    }

    #[test]
    fn duplicate_names_get_suffixes() {
        let mut taken = HashSet::new();
        assert_eq!(unique_name("a.wav".into(), &mut taken), "a.wav");
        assert_eq!(unique_name("A.WAV".into(), &mut taken), "A (2).WAV");
        assert_eq!(unique_name("a.wav".into(), &mut taken), "a (3).wav");
    }

    #[test]
    fn single_bank_uses_root() {
        let p = plan(cands(10), 1, 5, 1, None).unwrap();
        assert_eq!(p.files.len(), 5);
        assert!(p.files.iter().all(|f| f.bank.is_none() && !f.dest.contains('/')));
    }

    #[test]
    fn multiple_banks_are_even_and_numbered() {
        let p = plan(cands(100), 4, 10, 7, None).unwrap();
        assert_eq!(p.files.len(), 40);
        for b in 0..4u8 {
            let n = p.files.iter().filter(|f| f.bank == Some(b)).count();
            assert_eq!(n, 10);
        }
        assert!(p.files.iter().all(|f| f.dest.starts_with(&format!("{:02}/", f.bank.unwrap()))));
    }

    #[test]
    fn limits_are_enforced() {
        let p = plan(cands(2000), 16, 250, 3, None).unwrap();
        assert_eq!(p.files.len(), MAX_FILES_TOTAL);
        assert!(p.warnings.iter().any(|w| w.contains("800")));
        assert!(plan(cands(5), 0, 1, 1, None).is_err());
        assert!(plan(cands(5), 17, 1, 1, None).is_err());
        assert!(plan(cands(5), 1, 0, 1, None).is_err());
    }

    #[test]
    fn same_seed_same_plan_different_seed_differs() {
        let a = plan(cands(60), 3, 10, 42, None).unwrap();
        let mut reversed = cands(60);
        reversed.reverse();
        let b = plan(reversed, 3, 10, 42, None).unwrap();
        assert_eq!(a.files, b.files);
        let c = plan(cands(60), 3, 10, 43, None).unwrap();
        assert_ne!(a.files, c.files);
    }

    #[test]
    fn short_pool_warns_and_oversize_files_are_dropped() {
        let p = plan(cands(3), 2, 10, 1, None).unwrap();
        assert_eq!(p.files.len(), 3);
        assert!(p.warnings.iter().any(|w| w.contains("Only 3")));

        let mut c = cands(2);
        c[0].size_bytes = FAT32_MAX_FILE_BYTES + 1;
        let p = plan(c, 1, 5, 1, Some(FAT32_MAX_FILE_BYTES)).unwrap();
        assert_eq!(p.files.len(), 1);
    }

    #[test]
    fn copy_reports_progress_and_cancel_removes_partial_file() {
        let dir = std::env::temp_dir().join(format!("rmm-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("big.raw");
        std::fs::write(&src, vec![7u8; 3 * COPY_CHUNK + 5]).unwrap();

        let mut seen = Vec::new();
        let ok = dir.join("ok.raw");
        copy_file(&src, &ok, |n| {
            seen.push(n);
            true
        })
        .unwrap();
        assert_eq!(seen.len(), 4, "one callback per chunk");
        assert_eq!(*seen.last().unwrap(), (3 * COPY_CHUNK + 5) as u64);
        assert_eq!(std::fs::metadata(&ok).unwrap().len(), (3 * COPY_CHUNK + 5) as u64);

        let cancelled = dir.join("cancelled.raw");
        let err = copy_file(&src, &cancelled, |n| n < COPY_CHUNK as u64 + 1).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
        assert!(!cancelled.exists(), "partial file is removed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn planned(dest: &str, size: u64, bank: Option<u8>) -> PlannedFile {
        PlannedFile { src: String::new(), dest: dest.into(), size_bytes: size, bank }
    }

    /// A card with three audio files plus things that must never be touched.
    fn messy_card(name: &str) -> PathBuf {
        let card = std::env::temp_dir().join(format!("rmm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&card);
        for d in ["00", "01", "unnumbered"] {
            std::fs::create_dir_all(card.join(d)).unwrap();
        }
        for (f, n) in [
            ("00/a.raw", 100),
            ("00/b.raw", 100),
            ("01/c.raw", 100),
            ("settings.txt", 10),
            (".hidden.raw", 100),
            ("unnumbered/x.raw", 100),
        ] {
            std::fs::write(card.join(f), vec![0u8; n]).unwrap();
        }
        card
    }

    fn sample_plan() -> Plan {
        Plan {
            seed: 0,
            banks: 3,
            files: vec![
                planned("00/a.raw", 100, Some(0)), // identical to what is on the card
                planned("00/b.raw", 200, Some(0)), // same name, different size
                planned("02/new.raw", 50, Some(2)),
            ],
            total_bytes: 350,
            warnings: vec![],
        }
    }

    #[test]
    fn refuse_mode_blocks_a_card_with_audio() {
        let card = messy_card("refuse");
        assert!(compute_changes(&card, &sample_plan(), Existing::Refuse).is_err());
        std::fs::remove_dir_all(&card).unwrap();
    }

    #[test]
    fn replace_mode_deletes_all_firmware_visible_audio_only() {
        let card = messy_card("replace");
        let c = compute_changes(&card, &sample_plan(), Existing::Replace).unwrap();
        let mut del: Vec<_> = c.delete.iter().map(|(r, _)| r.as_str()).collect();
        del.sort();
        assert_eq!(del, vec!["00/a.raw", "00/b.raw", "01/c.raw"]);
        let p = preview_from(&c);
        assert_eq!((p.delete_files, p.copy_files, p.unchanged_files), (3, 3, 0));
        std::fs::remove_dir_all(&card).unwrap();
    }

    #[test]
    fn add_mode_keeps_everything_and_never_overwrites() {
        let card = messy_card("add");
        let c = compute_changes(&card, &sample_plan(), Existing::Add).unwrap();
        assert!(c.delete.is_empty(), "Add never deletes");
        assert_eq!(c.unchanged, 1, "a.raw is already there with the same size");
        let mut dests: Vec<_> = c.copies.iter().map(|f| f.dest.as_str()).collect();
        dests.sort();
        assert_eq!(dests, vec!["00/b (2).raw", "02/new.raw"], "b.raw clashes with a different file, so it is renamed");
        let p = preview_from(&c);
        assert_eq!((p.delete_files, p.copy_files, p.unchanged_files), (0, 2, 1));
        assert_eq!(p.copy_bytes, 250);
        std::fs::remove_dir_all(&card).unwrap();
    }

    #[test]
    fn add_mode_is_idempotent_even_after_renames() {
        let card = messy_card("idem");
        // Pretend an earlier Add stored a different b.raw (200 bytes) as "b (2).raw".
        std::fs::write(card.join("00/b (2).raw"), vec![0u8; 200]).unwrap();
        let c = compute_changes(&card, &sample_plan(), Existing::Add).unwrap();
        assert_eq!(c.unchanged, 2, "a.raw and the already-renamed b.raw");
        let dests: Vec<_> = c.copies.iter().map(|f| f.dest.as_str()).collect();
        assert_eq!(dests, vec!["02/new.raw"]);
        std::fs::remove_dir_all(&card).unwrap();
    }

    #[test]
    fn appledouble_stubs_are_removed_with_their_file() {
        let card = messy_card("stubs");
        std::fs::write(card.join("01/._c.raw"), [0u8; 8]).unwrap();
        let c = compute_changes(&card, &sample_plan(), Existing::Replace).unwrap();
        delete_audio(&card, &c.delete, |_, _| {}).unwrap();
        assert!(!card.join("01").exists(), "bank folder is pruned once the stub is gone too");
        std::fs::remove_dir_all(&card).unwrap();
    }

    #[test]
    fn add_mode_counts_existing_files_toward_limits() {
        let card = std::env::temp_dir().join(format!("rmm-addlimit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&card);
        std::fs::create_dir_all(card.join("00")).unwrap();
        for i in 0..MAX_FILES_PER_BANK {
            std::fs::write(card.join(format!("00/f{i:03}.raw")), [0u8; 4]).unwrap();
        }
        let plan = Plan {
            seed: 0,
            banks: 2,
            files: vec![planned("00/extra.raw", 4, Some(0)), planned("01/ok.raw", 4, Some(1))],
            total_bytes: 8,
            warnings: vec![],
        };
        let err = compute_changes(&card, &plan, Existing::Add).err().expect("over the bank limit");
        assert!(err.contains("bank 00") && err.contains("250"), "{err}");
        // Erasing first makes the same plan fine.
        assert!(compute_changes(&card, &plan, Existing::Replace).is_ok());
        std::fs::remove_dir_all(&card).unwrap();
    }

    #[test]
    fn add_mode_warns_when_layouts_differ() {
        let card = messy_card("layout");
        // Existing audio is in bank folders; this plan puts files in the root.
        let plan = Plan { seed: 0, banks: 1, files: vec![planned("root.raw", 10, None)], total_bytes: 10, warnings: vec![] };
        let c = compute_changes(&card, &plan, Existing::Add).unwrap();
        assert_eq!(c.warnings.len(), 1);
        std::fs::remove_dir_all(&card).unwrap();
    }

    #[test]
    fn deleting_prunes_empty_banks_but_never_other_files() {
        let card = messy_card("prune");
        let c = compute_changes(&card, &sample_plan(), Existing::Replace).unwrap();
        let mut seen = Vec::new();
        delete_audio(&card, &c.delete, |i, rel| seen.push((i, rel.to_string()))).unwrap();
        assert_eq!(seen.len(), 3);
        assert!(!card.join("00").exists() && !card.join("01").exists(), "emptied bank folders are removed");
        assert!(card.join("settings.txt").exists());
        assert!(card.join(".hidden.raw").exists());
        assert!(card.join("unnumbered/x.raw").exists(), "unnumbered folders are never touched");
        assert!(card.exists());
        std::fs::remove_dir_all(&card).unwrap();
    }

    #[test]
    fn a_bank_with_other_files_is_not_removed() {
        let card = messy_card("keepdir");
        std::fs::write(card.join("01/notes.txt"), b"keep me").unwrap();
        let c = compute_changes(&card, &sample_plan(), Existing::Replace).unwrap();
        delete_audio(&card, &c.delete, |_, _| {}).unwrap();
        assert!(card.join("01/notes.txt").exists());
        assert!(!card.join("01/c.raw").exists());
        std::fs::remove_dir_all(&card).unwrap();
    }

    #[test]
    fn rejects_unsafe_destinations() {
        let card = Path::new("/Volumes/CARD");
        assert!(safe_dest(card, "../evil.wav").is_err());
        assert!(safe_dest(card, "/etc/passwd").is_err());
        assert!(safe_dest(card, "").is_err());
        assert_eq!(safe_dest(card, "03/a.wav").unwrap(), card.join("03/a.wav"));
    }

    #[test]
    fn write_copies_files_and_refuses_nonempty_cards() {
        let base = std::env::temp_dir().join(format!("rmm-card-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut cs = Vec::new();
        for i in 0..4 {
            let p = src.join(format!("s{i}.raw"));
            std::fs::write(&p, vec![i as u8; 2_000]).unwrap();
            cs.push(Candidate { path: p.to_string_lossy().into_owned(), size_bytes: 2_000 });
        }
        let p = plan(cs, 2, 2, 5, None).unwrap();

        // A temp dir isn't a mounted volume root, so `write` must refuse it.
        let card = base.join("card");
        std::fs::create_dir_all(&card).unwrap();
        assert!(write(&card, &p, Existing::Refuse, |_| {}).unwrap_err().contains("mounted volume"));

        // The copy primitive itself preserves bytes.
        let dest = card.join("x.raw");
        copy_file(Path::new(&p.files[0].src), &dest, |_| true).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&p.files[0].src).unwrap());

        // Now the card has audio: refused before any volume check.
        let err = write(&card, &p, Existing::Refuse, |_| {}).unwrap_err();
        assert!(err.contains("already has 1 audio file"), "{err}");
        std::fs::remove_dir_all(&base).unwrap();
    }
}

#[cfg(test)]
mod real_volume {
    use super::*;

    /// Manual: `RMM_TEST_CARD=/Volumes/X cargo test real_volume -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn write_to_real_volume() {
        let card = std::env::var("RMM_TEST_CARD").expect("set RMM_TEST_CARD");
        let card = Path::new(&card);
        let src = std::env::temp_dir().join("rmm-real-src");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(&src).unwrap();
        let mut cs = Vec::new();
        let size: usize = std::env::var("RMM_TEST_FILE_BYTES").ok().and_then(|v| v.parse().ok()).unwrap_or(50_000);
        for i in 0..6 {
            let p = src.join(format!("s{i}.raw"));
            std::fs::write(&p, vec![i as u8; size]).unwrap();
            cs.push(Candidate { path: p.to_string_lossy().into_owned(), size_bytes: size as u64 });
        }
        let seed: u64 = std::env::var("RMM_TEST_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(9);
        let per_bank: usize = std::env::var("RMM_TEST_PER_BANK").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
        let mode = match std::env::var("RMM_TEST_MODE").as_deref() {
            Ok("replace") => Existing::Replace,
            Ok("add") => Existing::Add,
            _ => Existing::Refuse,
        };
        let p = plan(cs, 3, per_bank, seed, None).unwrap();
        println!("preview: {:?}", preview(card, &p, mode));
        let r = write(card, &p, mode, |pr| println!("progress {} {}/{} {} {}B", pr.phase, pr.done_files, pr.total_files, pr.current, pr.bytes_done));
        println!("result: {r:?}");
        if r.is_ok() && matches!(mode, Existing::Replace) {
            let mut on_card: Vec<String> = scan::scan(card).unwrap().files.into_iter().map(|f| f.relative_path).collect();
            let mut planned: Vec<String> = p.files.iter().map(|f| f.dest.clone()).collect();
            on_card.sort();
            planned.sort();
            assert_eq!(on_card, planned, "card must match the plan exactly");
            println!("card matches plan: {on_card:?}");
        }
        r.unwrap();
    }
}
