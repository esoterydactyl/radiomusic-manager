//! Plans and writes a randomized Radio Music card.
//!
//! Layout rules (see <https://www.musicthing.co.uk/Radio_Music_Reference/>):
//! one bank is just the card root; several banks are folders named `00`..`15`.
//! Limits: 16 banks, 250 files per bank, 800 files per card. Files whose names
//! start with `.` or `_` are ignored by the firmware, so we never produce them.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
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
    pub done: usize,
    pub total: usize,
    pub current: String,
}

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

fn copy_synced(src: &Path, dest: &Path) -> std::io::Result<()> {
    let mut reader = BufReader::new(File::open(src)?);
    let out = File::create(dest)?;
    let mut writer = BufWriter::new(out);
    std::io::copy(&mut reader, &mut writer)?;
    writer.flush()?;
    // Make sure the bytes are on the card, not just in the OS cache.
    writer.get_ref().sync_all()
}

/// Copy the plan onto `card`. Never deletes anything: refuses a card that already holds audio.
pub fn write(card: &Path, plan: &Plan, mut on_progress: impl FnMut(Progress)) -> Result<usize, String> {
    validate(plan)?;
    if !card.is_dir() {
        return Err(format!("{} is not a directory", card.display()));
    }

    let existing = scan::scan(card)?;
    if !existing.files.is_empty() {
        return Err(format!(
            "The card already has {} audio file(s). Remove them first; this tool never deletes your files.",
            existing.files.len()
        ));
    }

    let volume = volumes::list()
        .into_iter()
        .find(|v| Path::new(&v.mount_point) == card)
        .ok_or("That folder is not a mounted volume's root")?;
    if plan.total_bytes > volume.available_bytes {
        return Err(format!(
            "Not enough space: need {} MB, card has {} MB free",
            plan.total_bytes / 1_000_000,
            volume.available_bytes / 1_000_000
        ));
    }

    let total = plan.files.len();
    for (i, f) in plan.files.iter().enumerate() {
        let dest = safe_dest(card, &f.dest)?;
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        }
        copy_synced(Path::new(&f.src), &dest)
            .map_err(|e| format!("Copying {} failed after {i} of {total} files: {e}", f.src))?;
        on_progress(Progress { done: i + 1, total, current: f.dest.clone() });
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
        assert!(write(&card, &p, |_| {}).unwrap_err().contains("mounted volume"));

        // The copy primitive itself preserves bytes.
        let dest = card.join("x.raw");
        copy_synced(Path::new(&p.files[0].src), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), std::fs::read(&p.files[0].src).unwrap());

        // Now the card has audio: refused before any volume check.
        let err = write(&card, &p, |_| {}).unwrap_err();
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
        for i in 0..6 {
            let p = src.join(format!("s{i}.raw"));
            std::fs::write(&p, vec![i as u8; 50_000]).unwrap();
            cs.push(Candidate { path: p.to_string_lossy().into_owned(), size_bytes: 50_000 });
        }
        let p = plan(cs, 3, 2, 9, None).unwrap();
        println!("vols: {:?}", crate::volumes::list().iter().map(|v| (&v.mount_point, &v.file_system, v.available_bytes)).collect::<Vec<_>>());
        let r = write(card, &p, |pr| println!("progress {} / {} {}", pr.done, pr.total, pr.current));
        println!("result: {r:?}");
        r.unwrap();
    }
}
