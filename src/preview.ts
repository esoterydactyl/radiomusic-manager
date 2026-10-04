// Audio preview dock: waveform, playback and start/end trim markers for one file at a time.
//
// The Radio Music plays only the left channel of a stereo file, so the waveform and the playback
// are the left channel too. Playback is fed from the backend in short WAV windows, so a file that
// is hundreds of megabytes never has to be loaded whole.

import { invoke } from "@tauri-apps/api/core";
import { parseDuration } from "./filters";

interface Trim {
  start: number;
  end: number;
}

/** Trim chosen per source file (keyed by path). Only files with a real trim are in here. */
export const trims = new Map<string, Trim>();

export function trimsSignature(): string {
  return JSON.stringify([...trims.entries()]);
}

interface AudioInfo {
  format: string;
  duration_secs: number;
  rate: number;
  channels: number;
  bits: number;
  peaks: [number, number][];
}

interface LevelsInfo {
  peak_db: number;
  rms_db: number;
  gain_db: number;
}

export type NormalizeChoice = "off" | "peak" | "rms";

interface Options {
  /** Called when a trim is committed, so tables and plans can update. */
  onTrimChange: () => void;
  getNormalize: () => NormalizeChoice;
}

const WAVE_HEIGHT = 64;
const BUCKETS = 1200;
const WINDOW_SECS = 60;
const MIN_TRIM_SECS = 0.01;
const HANDLE_GRAB_PX = 8;

const $ = <T extends HTMLElement>(sel: string) => document.querySelector(sel) as T;

let opts: Options = { onTrimChange: () => {}, getNormalize: () => "off" };
const infoCache = new Map<string, AudioInfo>();

let current: { path: string; label: string; info: AudioInfo } | null = null;
let loadingPath: string | null = null;
let dismissed = false;
let playhead = 0;
let playing = false;
let playToken = 0;
let audio: HTMLAudioElement | null = null;
let audioOffset = 0;
let raf = 0;
let levelsTimer = 0;
let drag: "start" | "end" | null = null;

/** Show or hide the dock, and let the page make room for it (or take the room back). */
function setDockHidden(hidden: boolean) {
  $("#preview-dock").hidden = hidden;
  document.getElementById("app")?.classList.toggle("dock-open", !hidden);
}

export function currentPreviewPath(): string | null {
  return current?.path ?? loadingPath;
}

function fmtTime(secs: number): string {
  const s = Math.max(0, secs);
  const m = Math.floor(s / 60);
  return `${m}:${(s - m * 60).toFixed(1).padStart(4, "0")}`;
}

export function trimLabel(t: Trim): string {
  return `${fmtTime(t.start)}–${fmtTime(t.end)}`;
}

function trimOf(path: string, duration: number): Trim {
  return trims.get(path) ?? { start: 0, end: duration };
}

// ---- waveform -------------------------------------------------------------------------------

function draw() {
  const canvas = $<HTMLCanvasElement>("#pv-canvas");
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth;
  if (w === 0) return;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(WAVE_HEIGHT * dpr)) {
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(WAVE_HEIGHT * dpr);
  }
  const g = canvas.getContext("2d")!;
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  const css = getComputedStyle(document.documentElement);
  const color = (name: string) => css.getPropertyValue(name).trim();

  g.fillStyle = color("--bg-warm") || "#100E12";
  g.fillRect(0, 0, w, WAVE_HEIGHT);
  if (!current) return;

  const { peaks, duration_secs: dur } = current.info;
  const mid = WAVE_HEIGHT / 2;
  g.fillStyle = color("--ink-faint") || "#8F8A82";
  for (let x = 0; x < w; x++) {
    const [mn, mx] = peaks[Math.min(peaks.length - 1, Math.floor((x / w) * peaks.length))] ?? [0, 0];
    const y1 = mid - Math.max(0, mx) * mid * 0.95;
    const y2 = mid - Math.min(0, mn) * mid * 0.95;
    g.fillRect(x, y1, 1, Math.max(1, y2 - y1));
  }

  const t = trimOf(current.path, dur);
  const xs = (t.start / dur) * w;
  const xe = (t.end / dur) * w;
  // Dim whatever the trim cuts off.
  g.fillStyle = "rgba(14, 12, 15, 0.72)";
  g.fillRect(0, 0, xs, WAVE_HEIGHT);
  g.fillRect(xe, 0, w - xe, WAVE_HEIGHT);

  g.fillStyle = color("--ink") || "#E9E2D4";
  g.fillRect(xs - 1, 0, 2, WAVE_HEIGHT);
  g.fillRect(xe - 1, 0, 2, WAVE_HEIGHT);
  g.fillRect(xs - 1, 0, 9, 9); // flag pointing into the kept region
  g.fillRect(xe - 8, 0, 9, 9);

  g.fillStyle = color("--alive") || "#A6C155";
  g.fillRect(Math.round((playhead / dur) * w), 0, 1, WAVE_HEIGHT);
}

function secsAt(clientX: number): number {
  if (!current) return 0;
  const rect = $<HTMLCanvasElement>("#pv-canvas").getBoundingClientRect();
  const frac = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
  return frac * current.info.duration_secs;
}

function handleNear(clientX: number): "start" | "end" | null {
  if (!current) return null;
  const rect = $<HTMLCanvasElement>("#pv-canvas").getBoundingClientRect();
  const t = trimOf(current.path, current.info.duration_secs);
  const dur = current.info.duration_secs;
  const dStart = Math.abs(clientX - (rect.left + (t.start / dur) * rect.width));
  const dEnd = Math.abs(clientX - (rect.left + (t.end / dur) * rect.width));
  if (Math.min(dStart, dEnd) > HANDLE_GRAB_PX) return null;
  return dStart <= dEnd ? "start" : "end";
}

// ---- trim -----------------------------------------------------------------------------------

function syncInputs() {
  if (!current) return;
  const t = trimOf(current.path, current.info.duration_secs);
  $<HTMLInputElement>("#pv-start").value = fmtTime(t.start);
  $<HTMLInputElement>("#pv-end").value = fmtTime(t.end);
  ($("#pv-reset") as HTMLButtonElement).disabled = !trims.has(current.path);
  $("#pv-time").textContent = `${fmtTime(playhead)} / ${fmtTime(current.info.duration_secs)}`;
}

function setTrim(start: number, end: number, commit: boolean) {
  if (!current) return;
  const dur = current.info.duration_secs;
  let s = Math.min(Math.max(0, start), dur - MIN_TRIM_SECS);
  let e = Math.max(Math.min(dur, end), s + MIN_TRIM_SECS);
  s = Math.min(s, e - MIN_TRIM_SECS);
  // A trim covering the whole file is no trim at all.
  if (s <= 0.0005 && e >= dur - 0.0005) trims.delete(current.path);
  else trims.set(current.path, { start: s, end: e });
  if (playhead < s || playhead > e) playhead = s;
  syncInputs();
  draw();
  scheduleLevels();
  if (commit) opts.onTrimChange();
}

function scheduleLevels() {
  window.clearTimeout(levelsTimer);
  levelsTimer = window.setTimeout(() => void refreshLevels(), 250);
}

/** Show peak/loudness of the kept region and what the chosen normalize mode would do to it. */
export async function refreshLevels() {
  if (!current) {
    $("#pv-stats").textContent = "";
    return;
  }
  const path = current.path;
  const trim = trims.get(path) ?? null;
  const choice = opts.getNormalize();
  try {
    const l = await invoke<LevelsInfo>("audio_levels", { path, trim, mode: choice === "off" ? null : choice });
    if (current?.path !== path) return;
    const db = (v: number) => (Number.isFinite(v) ? `${v.toFixed(1)} dB` : "silent");
    $("#pv-stats").textContent =
      `peak ${db(l.peak_db)} · RMS ${db(l.rms_db)}` +
      (choice === "off" || !Number.isFinite(l.peak_db) ? "" : ` · normalize ${l.gain_db >= 0 ? "+" : ""}${l.gain_db.toFixed(1)} dB`);
  } catch {
    $("#pv-stats").textContent = "";
  }
}

// ---- playback -------------------------------------------------------------------------------

function stopPlayback() {
  playToken++;
  cancelAnimationFrame(raf);
  if (audio) {
    audio.pause();
    URL.revokeObjectURL(audio.src);
    audio = null;
  }
  playing = false;
  $("#pv-play").textContent = "Play";
}

function tick() {
  if (!playing || !audio) return;
  playhead = audioOffset + audio.currentTime;
  syncInputs();
  draw();
  raf = requestAnimationFrame(tick);
}

async function playWindow(from: number, limit: number, token: number) {
  if (!current) return;
  const span = Math.min(WINDOW_SECS, limit - from);
  if (span <= 0.02) {
    stopPlayback();
    return;
  }
  const path = current.path;
  let bytes: ArrayBuffer;
  try {
    bytes = await invoke<ArrayBuffer>("audio_preview", { path, startSecs: from, maxSecs: span });
  } catch (err) {
    if (token === playToken) {
      stopPlayback();
      $("#pv-info").textContent = `Could not play: ${err}`;
    }
    return;
  }
  if (token !== playToken || current?.path !== path) return;

  const url = URL.createObjectURL(new Blob([bytes], { type: "audio/wav" }));
  const el = new Audio(url);
  audio = el;
  audioOffset = from;
  el.addEventListener("ended", () => {
    URL.revokeObjectURL(url);
    if (token !== playToken) return;
    if (from + span < limit - 0.02) void playWindow(from + span, limit, token);
    else {
      stopPlayback();
      playhead = trimOf(path, current?.info.duration_secs ?? 0).start;
      syncInputs();
      draw();
    }
  });
  try {
    await el.play();
  } catch (err) {
    if (token === playToken) {
      stopPlayback();
      $("#pv-info").textContent = `Could not play: ${err}`;
    }
    return;
  }
  playing = true;
  $("#pv-play").textContent = "Stop";
  cancelAnimationFrame(raf);
  raf = requestAnimationFrame(tick);
}

/** Play from `from` to the end marker (or the end of the file, if `from` is already past it). */
function playFrom(from: number) {
  if (!current) return;
  stopPlayback();
  const dur = current.info.duration_secs;
  const t = trimOf(current.path, dur);
  const limit = from < t.end - 0.02 ? t.end : dur;
  playhead = from;
  syncInputs();
  draw();
  void playWindow(from, limit, playToken);
}

// ---- public API -----------------------------------------------------------------------------

export async function openPreview(path: string, label: string) {
  stopPlayback();
  dismissed = false;
  loadingPath = path;
  current = null;
  $("#pv-name").textContent = label;
  $("#pv-info").textContent = "Reading waveform…";
  $("#pv-stats").textContent = "";
  setControlsEnabled(false);
  setDockHidden(false);
  draw();

  try {
    let info = infoCache.get(path);
    if (!info) {
      info = await invoke<AudioInfo>("audio_peaks", { path, buckets: BUCKETS });
      infoCache.set(path, info);
    }
    if (loadingPath !== path) return; // another file was chosen meanwhile
    current = { path, label, info };
    const t = trimOf(path, info.duration_secs);
    playhead = t.start;
    $("#pv-info").textContent =
      `${info.format} · ${fmtTime(info.duration_secs)} · ${info.rate} Hz · ${info.bits}-bit · ${info.channels === 1 ? "mono" : info.channels === 2 ? "stereo, left channel plays" : `${info.channels} ch`}`;
    setControlsEnabled(true);
    syncInputs();
    draw();
    void refreshLevels();
  } catch (err) {
    if (loadingPath !== path) return;
    $("#pv-info").textContent = `Can't preview this file: ${err}`;
    draw();
  }
}

export function closePreview() {
  stopPlayback();
  current = null;
  loadingPath = null;
  dismissed = true;
  setDockHidden(true);
}

/** Show the dock only on the tabs where it helps, and only while a file is open. */
export function syncDock(step: string, settingsOnly: boolean) {
  const wanted = (current !== null || loadingPath !== null) && !dismissed && !settingsOnly && (step === "files" || step === "build");
  setDockHidden(!wanted);
  if (wanted) draw();
  if (!wanted && playing) stopPlayback();
}

function setControlsEnabled(on: boolean) {
  for (const id of ["#pv-play", "#pv-start", "#pv-end", "#pv-start-here", "#pv-end-here", "#pv-reset"]) {
    ($(id) as HTMLButtonElement | HTMLInputElement).disabled = !on;
  }
  if (!on) $("#pv-time").textContent = "";
}

export function initPreview(options: Options) {
  opts = options;
  const canvas = $<HTMLCanvasElement>("#pv-canvas");

  $("#pv-close").addEventListener("click", closePreview);
  $("#pv-play").addEventListener("click", () => {
    if (!current) return;
    if (playing) stopPlayback();
    else {
      const t = trimOf(current.path, current.info.duration_secs);
      playFrom(playhead >= t.start && playhead < t.end - 0.05 ? playhead : t.start);
    }
  });
  $("#pv-reset").addEventListener("click", () => {
    if (!current) return;
    trims.delete(current.path);
    playhead = 0;
    syncInputs();
    draw();
    scheduleLevels();
    opts.onTrimChange();
  });
  $("#pv-start-here").addEventListener("click", () => {
    if (!current) return;
    const t = trimOf(current.path, current.info.duration_secs);
    setTrim(playhead, Math.max(t.end, playhead + MIN_TRIM_SECS), true);
  });
  $("#pv-end-here").addEventListener("click", () => {
    if (!current) return;
    const t = trimOf(current.path, current.info.duration_secs);
    setTrim(Math.min(t.start, playhead - MIN_TRIM_SECS), playhead, true);
  });
  for (const [id, which] of [["#pv-start", "start"], ["#pv-end", "end"]] as const) {
    $(id).addEventListener("change", () => {
      if (!current) return;
      const v = parseDuration($<HTMLInputElement>(id).value);
      const t = trimOf(current.path, current.info.duration_secs);
      if (v === null) {
        syncInputs();
        return;
      }
      setTrim(which === "start" ? v : t.start, which === "end" ? v : t.end, true);
    });
  }

  canvas.addEventListener("pointerdown", (e) => {
    if (!current) return;
    drag = handleNear(e.clientX);
    canvas.setPointerCapture(e.pointerId);
    if (!drag) playFrom(secsAt(e.clientX));
  });
  canvas.addEventListener("pointermove", (e) => {
    if (!current) return;
    if (!drag) {
      canvas.style.cursor = handleNear(e.clientX) ? "ew-resize" : "pointer";
      return;
    }
    const t = trimOf(current.path, current.info.duration_secs);
    const at = secsAt(e.clientX);
    if (drag === "start") setTrim(Math.min(at, t.end - MIN_TRIM_SECS), t.end, false);
    else setTrim(t.start, Math.max(at, t.start + MIN_TRIM_SECS), false);
  });
  const release = () => {
    if (drag) {
      drag = null;
      opts.onTrimChange();
    }
  };
  canvas.addEventListener("pointerup", release);
  canvas.addEventListener("pointercancel", release);

  // Escape closes the dock, unless the key is meant for a text field.
  document.addEventListener("keydown", (e) => {
    const typing = e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement || e.target instanceof HTMLTextAreaElement;
    if (e.key === "Escape" && !typing && !$("#preview-dock").hidden) closePreview();
  });
  window.addEventListener("resize", draw);
  setControlsEnabled(false);
}
