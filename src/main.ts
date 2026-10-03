import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";

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
  td.title = text; // full text on hover when the cell is truncated
  if (cls) td.className = cls;
  return td;
}

function specString(f: AudioFileInfo): string {
  return [f.sample_rate && `${f.sample_rate} Hz`, f.bit_depth && `${f.bit_depth}-bit`, f.channels && `${f.channels}ch`]
    .filter(Boolean)
    .join(" ");
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
      tr.append(
        cell(f.bank === null ? "root" : String(f.bank), "dim"),
        cell(f.relative_path),
        cell(f.tags.title ?? ""),
        cell(f.tags.artist ?? ""),
        cell(f.duration_secs === null ? "" : `${f.duration_secs.toFixed(1)}s`, "dim"),
        cell(specString(f), "dim"),
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
const SOURCE_KEY = "radiomusic-manager:source";
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

interface SourceFile extends AudioFileInfo {
  eligible: boolean;
}

interface SourceScan {
  files: SourceFile[];
  eligible_count: number;
  unsupported_by_extension: Record<string, number>;
}

let sourceDir: string | null = null;
let sourceScan: SourceScan | null = null;
let sourceScanning = false;
let sourceFolder = ""; // "" = whole library, else a relative folder path
const expanded = new Set<string>();

interface TreeNode {
  name: string;
  path: string;
  children: Map<string, TreeNode>;
  eligible: number;
  total: number;
}

function buildTree(files: SourceFile[]): TreeNode {
  const root: TreeNode = { name: "", path: "", children: new Map(), eligible: 0, total: 0 };
  for (const f of files) {
    const dirs = f.relative_path.split("/").slice(0, -1);
    let node = root;
    node.total++;
    if (f.eligible) node.eligible++;
    let path = "";
    for (const d of dirs) {
      path = path ? `${path}/${d}` : d;
      let child = node.children.get(d);
      if (!child) {
        child = { name: d, path, children: new Map(), eligible: 0, total: 0 };
        node.children.set(d, child);
      }
      node = child;
      node.total++;
      if (f.eligible) node.eligible++;
    }
  }
  return root;
}

function renderTreeNode(node: TreeNode, isRoot: boolean): HTMLLIElement {
  const li = document.createElement("li");
  const row = document.createElement("div");
  const hasKids = node.children.size > 0;
  const open = isRoot || expanded.has(node.path);
  row.className = `row${node.path === sourceFolder ? " selected" : ""}${node.eligible === 0 ? " empty" : ""}`;

  const twisty = Object.assign(document.createElement("span"), { className: "twisty", textContent: hasKids ? (open ? "−" : "+") : "" });
  const name = Object.assign(document.createElement("span"), { className: "name", textContent: isRoot ? "(all)" : node.name });
  const count = Object.assign(document.createElement("span"), { className: "count", textContent: String(node.eligible) });
  row.append(twisty, name, count);

  twisty.addEventListener("click", (e) => {
    if (!hasKids || isRoot) return;
    e.stopPropagation();
    if (expanded.has(node.path)) expanded.delete(node.path);
    else expanded.add(node.path);
    renderSource();
  });
  row.addEventListener("click", () => {
    sourceFolder = node.path;
    renderSource();
  });
  li.append(row);

  if (hasKids && open) {
    const ul = document.createElement("ul");
    [...node.children.values()]
      .sort((a, b) => a.name.localeCompare(b.name))
      .forEach((c) => ul.append(renderTreeNode(c, false)));
    li.append(ul);
  }
  return li;
}

function renderSource() {
  const showIneligible = $<HTMLInputElement>("#show-ineligible").checked;
  const summary = $("#source-summary");
  const body = $("#source-results");
  if (!sourceScan) {
    summary.textContent = "";
    body.replaceChildren();
    $("#source-tree").replaceChildren();
    return;
  }

  // A rescan may have removed the selected folder.
  if (sourceFolder !== "" && !sourceScan.files.some((f) => f.relative_path.startsWith(`${sourceFolder}/`))) {
    sourceFolder = "";
  }
  const tree = buildTree(sourceScan.files);
  const treeEl = $("#source-tree");
  const rootUl = document.createElement("ul");
  rootUl.append(renderTreeNode(tree, true));
  treeEl.replaceChildren(rootUl);
  const inFolder = (f: SourceFile) => sourceFolder === "" || f.relative_path.startsWith(`${sourceFolder}/`);

  const unsupported = Object.entries(sourceScan.unsupported_by_extension);
  const unsupportedTotal = unsupported.reduce((n, [, c]) => n + c, 0);
  const parts = [`${sourceScan.eligible_count} eligible of ${sourceScan.files.length} supported files`];
  if (unsupportedTotal > 0) {
    parts.push(`${unsupportedTotal} unusable (${unsupported.map(([e, c]) => `${c} .${e}`).join(", ")})`);
  }
  summary.textContent = parts.join(" · ");

  body.replaceChildren(
    ...sourceScan.files
      .filter((f) => inFolder(f) && (showIneligible || f.eligible))
      .map((f) => {
        const tr = document.createElement("tr");
        if (!f.eligible) tr.className = "ineligible";
        tr.append(
          cell(f.relative_path),
          cell(f.tags.title ?? ""),
          cell(f.tags.artist ?? ""),
          cell(f.duration_secs === null ? "" : `${f.duration_secs.toFixed(1)}s`, "dim"),
          cell(specString(f), "dim"),
          cell(f.warnings.join("; "), "warn"),
        );
        return tr;
      }),
  );
}

async function scanSource() {
  if (!sourceDir || sourceScanning) return;
  sourceScanning = true;
  $("#source-summary").textContent = "Scanning…";
  try {
    sourceScan = await invoke<SourceScan>("scan_source", { path: sourceDir });
    renderSource();
  } catch (err) {
    sourceScan = null;
    $("#source-results").replaceChildren();
    $("#source-summary").textContent = `Error: ${err}`;
  } finally {
    sourceScanning = false;
  }
}

function setSourceDir(dir: string | null) {
  if (dir !== sourceDir) {
    sourceFolder = "";
    expanded.clear();
  }
  sourceDir = dir;
  $("#source-path").textContent = dir ?? "No folder selected";
  ($("#rescan-source") as HTMLButtonElement).disabled = dir === null;
  try {
    if (dir) localStorage.setItem(SOURCE_KEY, dir);
  } catch {
    // storage unavailable; the folder just won't be remembered
  }
  void scanSource();
}

async function chooseSource() {
  const picked = await open({ directory: true, multiple: false, defaultPath: sourceDir ?? undefined });
  if (typeof picked === "string") setSourceDir(picked);
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

  $("#choose-source").addEventListener("click", () => void chooseSource());
  $("#rescan-source").addEventListener("click", () => void scanSource());
  $("#show-ineligible").addEventListener("change", renderSource);
  try {
    const last = localStorage.getItem(SOURCE_KEY);
    if (last) setSourceDir(last);
  } catch {
    // storage unavailable
  }

  void refreshVolumes();
  setInterval(() => void refreshVolumes(), POLL_MS);
});
