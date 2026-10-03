import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask, open } from "@tauri-apps/plugin-dialog";
import { compareBy, isFiltering, matches, NO_FILTERS, parseDuration, type Filters, type SortKey } from "./filters";

interface AudioFileInfo {
  path: string;
  relative_path: string;
  size_bytes: number;
  bank: number | null;
  format: string;
  duration_secs: number | null;
  sample_rate: number | null;
  bit_depth: number | null;
  channels: number | null;
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
      if (f.warnings.length > 0) tr.title = f.warnings.join("; ");
      tr.append(
        cell(f.bank === null ? "root" : String(f.bank), "dim"),
        cell(f.relative_path),
        cell(f.duration_secs === null ? "" : `${f.duration_secs.toFixed(1)}s`, "dim"),
        cell(specString(f), "dim"),
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
  updateBuildPanel();
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
  updateBuildPanel();
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
let filters: Filters = { ...NO_FILTERS };
let sort: { key: SortKey; dir: 1 | -1 } | null = null;
let sourceFolder = ""; // "" = whole library, else a relative folder path
const expanded = new Set<string>();

interface TreeNode {
  name: string;
  path: string;
  children: Map<string, TreeNode>;
  eligible: number;
  total: number;
}

/** `counts` decides which files contribute to each folder's displayed count. */
function buildTree(files: SourceFile[], counts: (f: SourceFile) => boolean): TreeNode {
  const root: TreeNode = { name: "", path: "", children: new Map(), eligible: 0, total: 0 };
  for (const f of files) {
    const dirs = f.relative_path.split("/").slice(0, -1);
    let node = root;
    node.total++;
    if (counts(f)) node.eligible++;
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
      if (counts(f)) node.eligible++;
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

const inFolder = (f: SourceFile) => sourceFolder === "" || f.relative_path.startsWith(`${sourceFolder}/`);
const passes = (f: SourceFile) => matches(f, filters);

/** Files a card may be drawn from: eligible, in the selected folder, and matching the filters. */
function poolFiles(): SourceFile[] {
  return (sourceScan?.files ?? []).filter((f) => f.eligible && inFolder(f) && passes(f));
}

function renderSource() {
  const showIneligible = $<HTMLInputElement>("#show-ineligible").checked;
  const summary = $("#source-summary");
  const body = $("#source-results");
  if (!sourceScan) {
    summary.textContent = "";
    body.replaceChildren();
    $("#source-tree").replaceChildren();
    updateBuildPanel();
    return;
  }

  // A rescan may have removed the selected folder.
  if (sourceFolder !== "" && !sourceScan.files.some((f) => f.relative_path.startsWith(`${sourceFolder}/`))) {
    sourceFolder = "";
  }
  const tree = buildTree(sourceScan.files, (f) => f.eligible && passes(f));
  const treeEl = $("#source-tree");
  const rootUl = document.createElement("ul");
  rootUl.append(renderTreeNode(tree, true));
  treeEl.replaceChildren(rootUl);

  const unsupported = Object.entries(sourceScan.unsupported_by_extension);
  const unsupportedTotal = unsupported.reduce((n, [, c]) => n + c, 0);
  const visible = sourceScan.files.filter(
    (f) => inFolder(f) && passes(f) && (showIneligible || f.eligible),
  );
  if (sort) visible.sort(compareBy(sort.key, sort.dir));
  const parts = [`${sourceScan.eligible_count} eligible of ${sourceScan.files.length} supported files`];
  if (isFiltering(filters) || sourceFolder !== "") parts.unshift(`${visible.length} shown`);
  if (unsupportedTotal > 0) {
    parts.push(`${unsupportedTotal} unusable (${unsupported.map(([e, c]) => `${c} .${e}`).join(", ")})`);
  }
  summary.textContent = parts.join(" · ");

  body.replaceChildren(
    ...visible.map((f) => {
        const tr = document.createElement("tr");
        if (!f.eligible) tr.className = "ineligible";
        if (f.warnings.length > 0) tr.title = f.warnings.join("; ");
        tr.append(
          cell(f.relative_path),
          cell(f.duration_secs === null ? "" : `${f.duration_secs.toFixed(1)}s`, "dim"),
          cell(specString(f), "dim"),
          );
        return tr;
      }),
  );

  syncFilterControls();
  updateBuildPanel();
}

/** Reflect filter/sort state in the controls (clear button, sort arrows). */
function syncFilterControls() {
  ($("#f-clear") as HTMLButtonElement).disabled = !isFiltering(filters);
  document.querySelectorAll<HTMLElement>("th[data-sort]").forEach((th) => {
    const active = sort?.key === th.dataset.sort;
    th.classList.toggle("sorted", active);
    th.classList.toggle("desc", active && sort?.dir === -1);
  });
}

function readFilters() {
  const min = $<HTMLInputElement>("#f-min");
  const max = $<HTMLInputElement>("#f-max");
  const minSecs = parseDuration(min.value);
  const maxSecs = parseDuration(max.value);
  // Non-blank but unparseable input is flagged and ignored rather than silently dropped.
  min.setAttribute("aria-invalid", String(min.value.trim() !== "" && minSecs === null));
  max.setAttribute("aria-invalid", String(max.value.trim() !== "" && maxSecs === null));
  filters = {
    search: $<HTMLInputElement>("#f-search").value,
    minSecs,
    maxSecs,
    format: $<HTMLSelectElement>("#f-format").value,
  };
  renderSource();
}

function clearFilters() {
  for (const id of ["#f-search", "#f-min", "#f-max"]) $<HTMLInputElement>(id).value = "";
  $<HTMLSelectElement>("#f-format").value = "";
  readFilters();
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

// ---- Build card -------------------------------------------------------------

interface PlannedFile {
  src: string;
  dest: string;
  size_bytes: number;
  bank: number | null;
}

interface Plan {
  seed: number;
  banks: number;
  files: PlannedFile[];
  total_bytes: number;
  warnings: string[];
}

let plan: Plan | null = null;
let planPoolKey = "";
let perBankEdited = false;
let writing = false;

const MAX_FILES_PER_BANK = 250;
const MAX_FILES_TOTAL = 800;

function poolKey(): string {
  return poolFiles()
    .map((f) => f.path)
    .join("\n");
}

function selectedVolume(): Volume | undefined {
  return volumes.find((v) => v.mount_point === selected);
}

function banksValue(): number {
  return Number($<HTMLInputElement>("#b-banks").value);
}

/** Parses files-per-bank; null (and a red border) if it isn't a whole number from 1 to 250. */
function perBankValue(): number | null {
  const el = $<HTMLInputElement>("#b-per");
  const t = el.value.trim();
  const n = /^\d+$/.test(t) ? Number(t) : NaN;
  const ok = n >= 1 && n <= MAX_FILES_PER_BANK;
  el.setAttribute("aria-invalid", String(!ok));
  return ok ? n : null;
}

function renderPlan() {
  const banksBody = $("#b-banks-body");
  const filesBody = $("#b-files-body");
  $("#b-warnings").replaceChildren(
    ...(plan?.warnings ?? []).map((w) => Object.assign(document.createElement("li"), { textContent: w })),
  );
  if (!plan) {
    banksBody.replaceChildren();
    filesBody.replaceChildren();
    return;
  }

  const perBank = new Map<number | null, { files: number; bytes: number }>();
  for (const f of plan.files) {
    const e = perBank.get(f.bank) ?? { files: 0, bytes: 0 };
    e.files++;
    e.bytes += f.size_bytes;
    perBank.set(f.bank, e);
  }
  banksBody.replaceChildren(
    ...[...perBank.entries()].map(([bank, e]) => {
      const tr = document.createElement("tr");
      tr.append(cell(bank === null ? "root" : String(bank).padStart(2, "0"), "dim"), cell(String(e.files)), cell(formatBytes(e.bytes), "dim"));
      return tr;
    }),
  );
  filesBody.replaceChildren(
    ...plan.files.map((f) => {
      const tr = document.createElement("tr");
      tr.append(cell(f.bank === null ? "root" : String(f.bank).padStart(2, "0"), "dim"), cell(f.dest), cell(f.src.replace(sourceDir ?? "", "").replace(/^\//, "")));
      return tr;
    }),
  );
}

function updateBuildPanel() {
  const banks = banksValue();
  $("#b-banks-out").textContent = String(banks);

  // Default files-per-bank spreads the 800-file card limit across the banks, until the user edits it.
  if (!perBankEdited) {
    $<HTMLInputElement>("#b-per").value = String(Math.min(MAX_FILES_PER_BANK, Math.floor(MAX_FILES_TOTAL / banks)));
  }
  const perBank = perBankValue();

  const pool = poolFiles();
  if (plan && poolKey() !== planPoolKey) {
    plan = null;
    renderPlan();
  }

  const wanted = perBank === null ? null : Math.min(banks * perBank, MAX_FILES_TOTAL);
  $("#b-pool").textContent = !sourceScan
    ? "Choose a source folder to draw samples from."
    : `Pool: ${pool.length} eligible file${pool.length === 1 ? "" : "s"} (what the source table shows)` +
      (wanted === null ? "" : ` · ${Math.min(wanted, pool.length)} will be used`);

  const vol = selectedVolume();
  const cardOk = !!vol && vol.warnings.length === 0;
  ($("#b-roll") as HTMLButtonElement).disabled = writing || pool.length === 0 || perBank === null;
  ($("#b-write") as HTMLButtonElement).disabled = writing || !plan || !cardOk;

  if (!writing && !$("#b-status").dataset.sticky) {
    $("#b-status").textContent = !plan
      ? ""
      : `${plan.files.length} files · ${formatBytes(plan.total_bytes)} · seed ${plan.seed}` +
        (cardOk ? ` · will be written to ${vol!.name || vol!.mount_point}` : vol ? " · selected card is not usable" : " · select a card to write");
  }
}

function setBuildStatus(text: string, sticky = false) {
  const el = $("#b-status");
  el.textContent = text;
  if (sticky) el.dataset.sticky = "1";
  else delete el.dataset.sticky;
}

async function roll() {
  const perBank = perBankValue();
  const pool = poolFiles();
  if (perBank === null || pool.length === 0) return;
  setBuildStatus("");
  try {
    plan = await invoke<Plan>("plan_card", {
      candidates: pool.map((f) => ({ path: f.path, size_bytes: f.size_bytes })),
      banks: banksValue(),
      filesPerBank: perBank,
      seed: crypto.getRandomValues(new Uint32Array(1))[0],
      cardFileSystem: selectedVolume()?.file_system ?? null,
    });
    planPoolKey = poolKey();
  } catch (err) {
    plan = null;
    setBuildStatus(`Error: ${err}`, true);
  }
  renderPlan();
  updateBuildPanel();
}

async function writePlan() {
  const vol = selectedVolume();
  if (!plan || !vol || writing) return;
  const label = vol.name || vol.mount_point;
  const go = await ask(
    `Copy ${plan.files.length} files (${formatBytes(plan.total_bytes)}) to ${label}? Existing files on the card are not touched.`,
    { title: "Write to card", kind: "info" },
  );
  if (!go) return;

  writing = true;
  updateBuildPanel();
  const unlisten = await listen<{ done: number; total: number; current: string }>("write-progress", (e) => {
    setBuildStatus(`Writing ${e.payload.done} / ${e.payload.total} · ${e.payload.current}`, true);
  });
  try {
    const n = await invoke<number>("write_card", { cardPath: vol.mount_point, plan });
    setBuildStatus(`Wrote ${n} files to ${label}. Safe to eject.`, true);
    plan = null;
    renderPlan();
    await scanSelected();
  } catch (err) {
    setBuildStatus(`Error: ${err}`, true);
  } finally {
    unlisten();
    writing = false;
    updateBuildPanel();
  }
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
  for (const id of ["#f-search", "#f-min", "#f-max"]) $(id).addEventListener("input", readFilters);
  for (const id of ["#f-format"]) $(id).addEventListener("change", readFilters);
  $("#f-clear").addEventListener("click", clearFilters);
  document.querySelectorAll<HTMLElement>("th[data-sort]").forEach((th) =>
    th.addEventListener("click", () => {
      const key = th.dataset.sort as SortKey;
      // asc -> desc -> unsorted
      sort = sort?.key !== key ? { key, dir: 1 } : sort.dir === 1 ? { key, dir: -1 } : null;
      renderSource();
    }),
  );
  try {
    const last = localStorage.getItem(SOURCE_KEY);
    if (last) setSourceDir(last);
  } catch {
    // storage unavailable
  }

  $("#b-banks").addEventListener("input", () => {
    delete $("#b-status").dataset.sticky;
    updateBuildPanel();
  });
  $("#b-per").addEventListener("input", () => {
    perBankEdited = true;
    delete $("#b-status").dataset.sticky;
    updateBuildPanel();
  });
  $("#b-roll").addEventListener("click", () => void roll());
  $("#b-write").addEventListener("click", () => void writePlan());
  updateBuildPanel();

  void refreshVolumes();
  setInterval(() => void refreshVolumes(), POLL_MS);
});
