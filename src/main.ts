import { invoke } from "@tauri-apps/api/core";

interface AudioFileInfo {
  relative_path: string;
  bank: number | null;
  format: string;
  duration_secs: number | null;
  sample_rate: number | null;
  bit_depth: number | null;
  channels: number | null;
  tags: { title?: string; artist?: string; album?: string };
  warnings: string[];
}

interface ScanResult {
  files: AudioFileInfo[];
  folder_count: number;
  warnings: string[];
}

function cell(text: string, cls?: string): HTMLTableCellElement {
  const td = document.createElement("td");
  td.textContent = text;
  if (cls) td.className = cls;
  return td;
}

function render(result: ScanResult) {
  document.querySelector("#scan-summary")!.textContent =
    `${result.files.length} audio files in ${result.folder_count} folders`;

  const warnings = document.querySelector("#scan-warnings")!;
  warnings.replaceChildren(
    ...result.warnings.map((w) => Object.assign(document.createElement("li"), { textContent: w })),
  );

  const table = document.querySelector("#scan-results")!;
  table.replaceChildren(
    ...result.files.map((f) => {
      const tr = document.createElement("tr");
      const spec = [f.sample_rate && `${f.sample_rate} Hz`, f.bit_depth && `${f.bit_depth}-bit`, f.channels && `${f.channels}ch`]
        .filter(Boolean)
        .join(" ");
      tr.append(
        cell(f.bank === null ? "root" : String(f.bank), "dim"),
        cell(f.relative_path),
        cell(f.tags.title ?? ""),
        cell(f.tags.artist ?? ""),
        cell(f.duration_secs === null ? "" : `${f.duration_secs.toFixed(1)}s`, "dim"),
        cell(spec, "dim"),
        cell(f.warnings.join("; "), "warn"),
      );
      return tr;
    }),
  );
}

interface Volume {
  name: string;
  mount_point: string;
  file_system: string;
  total_bytes: number;
  available_bytes: number;
  removable: boolean;
  warnings: string[];
}

const POLL_MS = 2000;
const PREFS_KEY = "radiomusic-manager:prefs";

let volumes: Volume[] = [];
let selected: string | null = null; // mount point
let scanning = false;

const $ = <T extends HTMLElement>(sel: string) => document.querySelector(sel) as T;

function loadPrefs(): { autoSelect: boolean; showAll: boolean } {
  try {
    return { autoSelect: true, showAll: false, ...JSON.parse(localStorage.getItem(PREFS_KEY) ?? "{}") };
  } catch {
    return { autoSelect: true, showAll: false };
  }
}

function savePrefs() {
  try {
    localStorage.setItem(
      PREFS_KEY,
      JSON.stringify({ autoSelect: $<HTMLInputElement>("#auto-select").checked, showAll: $<HTMLInputElement>("#show-all").checked }),
    );
  } catch {
    // storage unavailable; preferences just won't persist
  }
}

function formatBytes(n: number): string {
  return n >= 1e9 ? `${(n / 1e9).toFixed(1)} GB` : `${(n / 1e6).toFixed(0)} MB`;
}

function visibleVolumes(): Volume[] {
  const showAll = $<HTMLInputElement>("#show-all").checked;
  return volumes.filter((v) => showAll || v.removable);
}

function renderPicker() {
  const select = $<HTMLSelectElement>("#volume-select");
  const list = visibleVolumes();
  select.replaceChildren(
    ...(list.length === 0
      ? [new Option("No card detected", "")]
      : [new Option("Select a card…", ""), ...list.map((v) => new Option(`${v.name || "Untitled"} (${v.mount_point})`, v.mount_point))]),
  );
  select.value = list.some((v) => v.mount_point === selected) ? (selected as string) : "";

  const vol = list.find((v) => v.mount_point === selected);
  $("#card-info").textContent = vol
    ? `${vol.file_system || "unknown fs"} · ${formatBytes(vol.total_bytes)} · ${formatBytes(vol.available_bytes)} free`
    : "";
  $("#card-warnings").replaceChildren(
    ...(vol?.warnings ?? []).map((w) => Object.assign(document.createElement("li"), { textContent: w })),
  );
}

function clearResults() {
  $("#scan-summary").textContent = "";
  $("#scan-warnings").replaceChildren();
  $("#scan-results").replaceChildren();
}

async function scanSelected() {
  if (!selected || scanning) return;
  scanning = true;
  try {
    render(await invoke<ScanResult>("scan_directory", { path: selected }));
  } catch (err) {
    clearResults();
    $("#scan-summary").textContent = `Error: ${err}`;
  } finally {
    scanning = false;
  }
}

function select(mount: string | null) {
  if (mount === selected) return;
  selected = mount;
  renderPicker();
  clearResults();
  if (mount) void scanSelected();
}

async function refreshVolumes() {
  try {
    volumes = await invoke<Volume[]>("list_volumes");
  } catch (err) {
    $("#card-info").textContent = `Error listing volumes: ${err}`;
    return;
  }
  const list = visibleVolumes();

  // Selected card was removed.
  if (selected && !list.some((v) => v.mount_point === selected)) select(null);

  // Exactly one usable card present: pick it automatically.
  const usable = list.filter((v) => v.warnings.length === 0);
  if (!selected && $<HTMLInputElement>("#auto-select").checked && usable.length === 1) {
    select(usable[0].mount_point);
  }
  renderPicker();
}

window.addEventListener("DOMContentLoaded", () => {
  const prefs = loadPrefs();
  $<HTMLInputElement>("#auto-select").checked = prefs.autoSelect;
  $<HTMLInputElement>("#show-all").checked = prefs.showAll;

  $<HTMLSelectElement>("#volume-select").addEventListener("change", (e) => {
    select((e.target as HTMLSelectElement).value || null);
  });
  $("#rescan").addEventListener("click", () => void scanSelected());
  $("#auto-select").addEventListener("change", () => {
    savePrefs();
    void refreshVolumes();
  });
  $("#show-all").addEventListener("change", () => {
    savePrefs();
    void refreshVolumes();
  });

  void refreshVolumes();
  setInterval(() => void refreshVolumes(), POLL_MS);
});
