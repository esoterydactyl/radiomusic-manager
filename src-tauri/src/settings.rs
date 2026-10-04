//! Reads and writes the Radio Music's `settings.txt`.
//!
//! Format (from <https://www.musicthing.co.uk/Radio_Music_Reference/>): lines of
//! `<setting> = <whole number>`, `#` starts a comment, whitespace is ignored (even inside names)
//! and names are case-insensitive. A `settings.txt` in the card root gives defaults for every
//! bank. This module edits the root file only.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct ChoiceOpt {
    pub value: i64,
    pub label: &'static str,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Kind {
    Choice { options: Vec<ChoiceOpt> },
    Toggle,
    Number { min: i64, max: i64, unit: &'static str },
}

#[derive(Serialize, Clone, Debug)]
pub struct SettingDef {
    pub key: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub help: &'static str,
    #[serde(flatten)]
    pub kind: Kind,
    pub default: i64,
    /// Shown up front; everything else is under "More settings".
    pub common: bool,
}

fn choice(options: &[(i64, &'static str)]) -> Kind {
    Kind::Choice { options: options.iter().map(|&(value, label)| ChoiceOpt { value, label }).collect() }
}

fn num(min: i64, max: i64, unit: &'static str) -> Kind {
    Kind::Number { min, max, unit }
}

#[allow(clippy::too_many_arguments)]
fn def(key: &'static str, label: &'static str, group: &'static str, help: &'static str, kind: Kind, default: i64, common: bool) -> SettingDef {
    SettingDef { key, label, group, help, kind, default, common }
}

/// Every setting the firmware understands, in the order the UI shows them.
pub fn schema() -> Vec<SettingDef> {
    use Kind::Toggle;
    vec![
        // --- Playback ---
        def("tunerMode", "Tuner mode", "Playback", "How the Station knob moves between files.",
            choice(&[(0, "Sharp: files switch with a crossfade"), (1, "Soft: continuous fade between neighbours"), (2, "Radio: noise and distant stations")]), 0, true),
        def("loopMode", "Looping", "Playback", "What happens when a file reaches its end.",
            choice(&[(0, "Don't loop"), (1, "Loop back to the start"), (2, "Ping-pong")]), 1, true),
        def("highQuality", "Audio quality", "Playback", "Off is 8-bit; on is 24-bit 96kHz with cubic interpolation.",
            Toggle, 1, true),
        def("crossfadeTime", "Crossfade time", "Playback", "Length of the crossfade between files.",
            num(0, 60000, "ms"), 25, true),
        def("fadeMode", "Fade shape", "Playback", "Constant power suits audio; linear suits correlated signals.",
            choice(&[(0, "Constant power"), (1, "Linear")]), 0, false),
        def("radioStart", "Stations keep playing", "Playback", "On: stations advance in the background. Off: playback restarts when the station changes.",
            Toggle, 1, false),
        def("reselectSubdirOnStationChange", "Reselect subfolder on station change", "Playback", "On: a subfolder is picked again whenever the station changes. Off: only on reset.",
            Toggle, 0, false),
        // --- Radio tuner ---
        def("minRadioStationStrength", "Minimum station strength", "Radio tuner", "In Radio tuner mode, how strong the most distant stations are.",
            num(0, 256, ""), 128, false),
        def("ssbEffect", "Single side band effect", "Radio tuner", "Approximate single side band pitch shift.",
            Toggle, 0, false),
        def("whistleVol", "Whistle volume", "Radio tuner", "Heterodyne whistle effect, 0 to 100.",
            num(0, 100, ""), 50, false),
        def("noiseVol", "Noise volume", "Radio tuner", "Background hiss in Radio tuner mode, 0 to 100.",
            num(0, 100, ""), 50, false),
        // --- Pitch & speed ---
        def("speedMode", "Speed response", "Pitch & speed", "How the pitch knob and CV change playback speed.",
            choice(&[(0, "Tape: linear speed"), (1, "Notes: exponential, in semitones"), (2, "90s: time-stretch")]), 0, true),
        def("pitchKnobMin", "Pitch knob minimum", "Pitch & speed", "Notes mode: pitch at the far left of the knob.",
            num(-48, 24, "semitones"), -12, true),
        def("pitchKnobMax", "Pitch knob maximum", "Pitch & speed", "Notes mode: pitch at the far right of the knob.",
            num(-48, 24, "semitones"), 12, true),
        def("quantisePitchPot", "Quantise pitch knob", "Pitch & speed", "Notes mode: snap the knob to whole semitones.",
            Toggle, 1, false),
        def("quantisePitchCV", "Quantise pitch CV", "Pitch & speed", "Notes mode: snap the 1V/octave input to whole semitones.",
            Toggle, 1, false),
        def("pitchKnobLinearSpeedMin", "Knob speed minimum", "Pitch & speed", "Tape and 90s modes: speed at the far left of the knob.",
            num(-400, 400, "%"), -100, false),
        def("pitchKnobLinearSpeedMax", "Knob speed maximum", "Pitch & speed", "Tape and 90s modes: speed at the far right of the knob.",
            num(-400, 400, "%"), 100, false),
        def("pitchCVLinearSpeedMin", "CV speed minimum", "Pitch & speed", "Tape and 90s modes: speed at the lowest CV (0V).",
            num(-400, 400, "%"), -100, false),
        def("pitchCVLinearSpeedMax", "CV speed maximum", "Pitch & speed", "Tape and 90s modes: speed at the highest CV (5V).",
            num(-400, 400, "%"), 100, false),
        // --- Reset jack ---
        def("pulseMode", "Reset jack mode", "Reset jack", "0 makes it an input. A positive number makes it a pulse output multiplier; a negative one, a divisor.",
            num(-1000, 1000, ""), 0, false),
        def("pulseOutDividesCustomLoop", "Pulse follows loop window", "Reset jack", "On: pulses relate to the START/END loop window. Off: to the whole file.",
            Toggle, 1, false),
        def("resetJackPauses", "High reset pauses audio", "Reset jack", "Off: a rising edge restarts the sample. On: a high level pauses it.",
            Toggle, 0, false),
        def("resetDelay", "Reset delay", "Reset jack", "Delay before reacting to a rising edge on the reset jack.",
            num(0, 1_000_000, "µs"), 0, false),
        // --- Knobs & CV ---
        def("stationPotImmediate", "Station knob is immediate", "Knobs & CV", "On: moves take effect at once. Off: only on reset.",
            Toggle, 1, false),
        def("stationCVImmediate", "Station CV is immediate", "Knobs & CV", "On: moves take effect at once. Off: only on reset.",
            Toggle, 1, false),
        def("startPotImmediate", "Start knob is immediate", "Knobs & CV", "On: moves take effect at once. Off: only on reset.",
            Toggle, 0, false),
        def("startCVImmediate", "Start CV is immediate", "Knobs & CV", "On: moves take effect at once. Off: only on reset.",
            Toggle, 0, false),
        def("pitchPotImmediate", "Pitch knob is immediate", "Knobs & CV", "On: moves take effect at once. Off: only on reset.",
            Toggle, 1, false),
        def("pitchCVImmediate", "Pitch CV is immediate", "Knobs & CV", "On: moves take effect at once. Off: only on reset.",
            Toggle, 1, false),
        def("startCVDivider", "Start position steps", "Knobs & CV", "Round the start position to a multiple of this. 256 allows only the start, 1/4, 1/2 and 3/4.",
            num(1, 65536, ""), 2, false),
        // --- Display ---
        def("showMeter", "LED display", "Display", "What the LEDs show.",
            choice(&[(0, "Bank number"), (1, "Audio level meter"), (2, "Progress through the file")]), 1, true),
        def("meterHide", "Show bank number for", "Display", "After a bank change, how long the bank number stays on the LEDs.",
            num(0, 600_000, "ms"), 2000, false),
    ]
}

fn find(key: &str) -> Option<SettingDef> {
    let k = normalize_key(key);
    schema().into_iter().find(|d| normalize_key(d.key) == k)
}

/// Names ignore case and all whitespace.
pub fn normalize_key(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect()
}

/// `(normalised key, value)` for a `name = number` line, ignoring comments.
fn parse_line(line: &str) -> Option<(String, i64)> {
    let code = line.split('#').next()?;
    let (k, v) = code.split_once('=')?;
    let value: String = v.chars().filter(|c| !c.is_whitespace()).collect();
    let key = normalize_key(k);
    (!key.is_empty()).then_some(())?;
    Some((key, value.parse().ok()?))
}

pub fn validate(def: &SettingDef, value: i64) -> Result<(), String> {
    let ok = match &def.kind {
        Kind::Toggle => value == 0 || value == 1,
        Kind::Choice { options } => options.iter().any(|o| o.value == value),
        Kind::Number { min, max, .. } => (*min..=*max).contains(&value),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{} can't be {value}", def.label))
    }
}

#[derive(Serialize, Debug, Default, PartialEq)]
pub struct CardSettings {
    /// Whether `settings.txt` exists in the card root.
    pub exists: bool,
    /// Canonical setting name -> value, for the settings found in the file.
    pub values: BTreeMap<String, i64>,
    /// Names in the file that the app doesn't know. They are kept when saving.
    pub unknown: Vec<String>,
    /// Known settings whose value is outside the allowed range.
    pub warnings: Vec<String>,
}

pub fn parse(text: &str) -> CardSettings {
    let mut out = CardSettings { exists: true, ..Default::default() };
    for line in text.lines() {
        let Some((key, value)) = parse_line(line) else { continue };
        match find(&key) {
            Some(def) => {
                if let Err(e) = validate(&def, value) {
                    out.warnings.push(format!("{e} (allowed values are listed in the reference)"));
                }
                out.values.insert(def.key.to_string(), value);
            }
            None => out.unknown.push(key),
        }
    }
    out
}

pub fn read(card: &Path) -> Result<CardSettings, String> {
    let path = card.join("settings.txt");
    if !path.exists() {
        return Ok(CardSettings::default());
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("Could not read settings.txt: {e}"))?;
    Ok(parse(&String::from_utf8_lossy(&bytes)))
}

/// Apply `changes` to existing file text: update matching lines in place (keeping their comments),
/// leave everything else alone, and append settings that weren't in the file.
pub fn apply_changes(text: &str, changes: &BTreeMap<String, i64>) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let wanted: BTreeMap<String, (&str, i64)> = changes
        .iter()
        .filter_map(|(k, v)| find(k).map(|d| (normalize_key(d.key), (d.key, *v))))
        .collect();
    let mut written = std::collections::HashSet::new();
    let mut lines: Vec<String> = Vec::new();

    for line in text.lines() {
        let rewritten = parse_line(line).and_then(|(key, _)| {
            let (canonical, value) = wanted.get(&key)?;
            // Keep any trailing comment on the line.
            let comment = line.find('#').map(|i| format!(" {}", &line[i..])).unwrap_or_default();
            written.insert(key);
            Some(format!("{canonical} = {value}{comment}"))
        });
        lines.push(rewritten.unwrap_or_else(|| line.to_string()));
    }

    let missing: Vec<String> = wanted
        .iter()
        .filter(|(k, _)| !written.contains(*k))
        .map(|(_, (canonical, value))| format!("{canonical} = {value}"))
        .collect();
    if !missing.is_empty() {
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push("# Added by Radiomusic Manager".to_string());
        lines.extend(missing);
    }
    let mut out = lines.join(eol);
    out.push_str(eol);
    out
}

/// Validate and save `changes` into the card's root `settings.txt`.
/// Speed over safety: written in place with no flush (see the non-functional requirements).
pub fn write(card: &Path, changes: &BTreeMap<String, i64>) -> Result<(), String> {
    for (key, value) in changes {
        let def = find(key).ok_or_else(|| format!("Unknown setting: {key}"))?;
        validate(&def, *value)?;
    }
    let path = card.join("settings.txt");
    let existing = if path.exists() {
        String::from_utf8_lossy(&std::fs::read(&path).map_err(|e| format!("Could not read settings.txt: {e}"))?).into_owned()
    } else {
        String::new()
    };
    std::fs::write(&path, apply_changes(&existing, changes)).map_err(|e| format!("Could not write settings.txt: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
        pairs.iter().map(|&(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn schema_is_consistent() {
        let defs = schema();
        let mut keys: Vec<_> = defs.iter().map(|d| normalize_key(d.key)).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), defs.len(), "keys are unique");
        assert_eq!(defs.len(), 33, "every setting in the reference");
        for d in &defs {
            assert!(validate(d, d.default).is_ok(), "default of {} is valid", d.key);
        }
        assert!(defs.iter().filter(|d| d.common).count() >= 6);
    }

    #[test]
    fn schema_serialises_to_what_the_ui_expects() {
        let json: Vec<serde_json::Value> = schema().iter().map(|d| serde_json::to_value(d).unwrap()).collect();
        let by_key = |k: &str| json.iter().find(|j| j["key"] == k).unwrap();

        let tuner = by_key("tunerMode");
        assert_eq!(tuner["type"], "choice");
        assert_eq!(tuner["options"].as_array().unwrap().len(), 3);
        assert_eq!(tuner["options"][0]["value"], 0);
        assert_eq!(tuner["default"], 0);
        assert_eq!(tuner["common"], true);

        assert_eq!(by_key("highQuality")["type"], "toggle");
        let xf = by_key("crossfadeTime");
        assert_eq!((xf["type"].as_str(), xf["min"].as_i64(), xf["max"].as_i64(), xf["unit"].as_str()), (Some("number"), Some(0), Some(60000), Some("ms")));
        assert_eq!(by_key("pitchKnobMin")["default"], -12);
    }

    #[test]
    fn parses_the_documented_spellings() {
        assert_eq!(parse_line("Start Pot immediate = 1"), Some(("startpotimmediate".into(), 1)));
        assert_eq!(parse_line("STARTPOTIMMEDIATE=1"), Some(("startpotimmediate".into(), 1)));
        assert_eq!(parse_line("pitchKnobMin = -12   # semitones"), Some(("pitchknobmin".into(), -12)));
        assert_eq!(parse_line("# loopMode = 2"), None);
        assert_eq!(parse_line("loopMode ="), None);
        assert_eq!(parse_line("just text"), None);
        assert_eq!(parse_line("loopMode = two"), None);
    }

    #[test]
    fn reads_values_unknowns_and_range_warnings() {
        let s = parse("loopMode = 2\nbogus = 5\nTUNER MODE = 7\nhighQuality=0\n");
        assert_eq!(s.values.get("loopMode"), Some(&2));
        assert_eq!(s.values.get("highQuality"), Some(&0));
        assert_eq!(s.unknown, vec!["bogus".to_string()]);
        assert_eq!(s.warnings.len(), 1, "tunerMode 7 is out of range");
    }

    #[test]
    fn edits_in_place_keeping_comments_and_unknown_lines() {
        let before = "# my card\nloopMode = 1   # loop\nbogus = 9\nUser Notes = 3\ncrossfadeTime=25\n";
        let after = apply_changes(before, &changes(&[("loopMode", 2), ("crossfadeTime", 100), ("showMeter", 0)]));
        assert!(after.starts_with("# my card\n"));
        assert!(after.contains("loopMode = 2 # loop\n"), "comment kept: {after}");
        assert!(after.contains("crossfadeTime = 100\n"));
        assert!(after.contains("bogus = 9\n") && after.contains("User Notes = 3\n"), "unknown lines untouched");
        assert!(after.contains("# Added by Radiomusic Manager\nshowMeter = 0\n"), "new settings appended");
        assert_eq!(after.matches("loopMode").count(), 1);
    }

    #[test]
    fn matches_differently_spelled_lines_and_keeps_crlf() {
        let after = apply_changes("LOOP MODE=0\r\nfoo=1\r\n", &changes(&[("loopMode", 1)]));
        assert_eq!(after, "loopMode = 1\r\nfoo=1\r\n");
    }

    #[test]
    fn new_file_has_only_requested_settings() {
        let after = apply_changes("", &changes(&[("tunerMode", 1), ("loopMode", 0)]));
        assert_eq!(after, "# Added by Radiomusic Manager\nloopMode = 0\ntunerMode = 1\n");
        // And it round-trips.
        let back = parse(&after);
        assert_eq!(back.values.get("tunerMode"), Some(&1));
        assert_eq!(back.values.get("loopMode"), Some(&0));
    }

    #[test]
    fn rejects_unknown_and_out_of_range_values() {
        let dir = std::env::temp_dir().join(format!("rmm-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(write(&dir, &changes(&[("nope", 1)])).is_err());
        assert!(write(&dir, &changes(&[("tunerMode", 9)])).is_err());
        assert!(write(&dir, &changes(&[("highQuality", 2)])).is_err());
        assert!(!dir.join("settings.txt").exists(), "a rejected write leaves nothing behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_then_read_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("rmm-settings-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!read(&dir).unwrap().exists);
        write(&dir, &changes(&[("loopMode", 2), ("pitchKnobMin", -24)])).unwrap();
        write(&dir, &changes(&[("loopMode", 0)])).unwrap();
        let s = read(&dir).unwrap();
        assert!(s.exists);
        assert_eq!(s.values.get("loopMode"), Some(&0));
        assert_eq!(s.values.get("pitchKnobMin"), Some(&-24), "earlier values survive a later save");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
