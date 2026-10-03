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
  partition_scheme: string | null;
  partition_count: number | null;
  disk_size_bytes: number | null;
  formattable: boolean;
  format_blocker: string | null;
  format_fs: string | null;
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
    ? [
        vol.file_system || "unknown fs",
        formatBytes(vol.total_bytes),
        `${formatBytes(vol.available_bytes)} free`,
        vol.partition_scheme ? `${vol.partition_scheme} partition table` : null,
        vol.partition_count !== null ? `${vol.partition_count} partition${vol.partition_count === 1 ? "" : "s"}` : null,
        vol.disk_size_bytes && vol.disk_size_bytes !== vol.total_bytes ? `${formatBytes(vol.disk_size_bytes)} disk` : null,
      ]
        .filter(Boolean)
        .join(" · ")
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
  lastWriteOk = false;
  $("#fmt-confirm").hidden = true;
  $<HTMLInputElement>("#fmt-typed").value = "";
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
let writing = false;
let lastWriteOk = false;

type Existing = "refuse" | "replace" | "add";
const EXISTING_KEY = "radiomusic-manager:existing";

interface ChangePreview {
  delete_files: number;
  delete_bytes: number;
  copy_files: number;
  copy_bytes: number;
  unchanged_files: number;
  will_format: boolean;
  warnings: string[];
}

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

interface WriteProgress {
  phase: "formatting" | "deleting" | "writing";
  done_files: number;
  total_files: number;
  current: string;
  bank: number | null;
  bank_done: number;
  bank_total: number;
  bytes_done: number;
  bytes_total: number;
}

let progress: WriteProgress | null = null;

const bankLabel = (bank: number | null) => (bank === null ? "root" : String(bank).padStart(2, "0"));

/** Which bank's files the right-hand list shows: "all", "root", or a bank number as text. */
let bankSel = "all";
const bankKey = (bank: number | null) => (bank === null ? "root" : String(bank));

function selectBank(key: string) {
  bankSel = key;
  renderBankTable();
  renderFiles();
}

function renderBankTable() {
  const body = $("#b-banks-body");
  if (!plan) {
    body.replaceChildren();
    return;
  }
  const perBank = new Map<number | null, { files: number; bytes: number }>();
  for (const f of plan.files) {
    const e = perBank.get(f.bank) ?? { files: 0, bytes: 0 };
    e.files++;
    e.bytes += f.size_bytes;
    perBank.set(f.bank, e);
  }
  const order = [...perBank.keys()];
  const activeIdx = progress && progress.phase === "writing" ? order.indexOf(progress.bank) : -1;

  const row = (key: string, cells: HTMLTableCellElement[], extraClass = ""): HTMLTableRowElement => {
    const tr = document.createElement("tr");
    tr.className = `clickable ${extraClass} ${bankSel === key ? "bank-selected" : ""}`.trim();
    tr.tabIndex = 0;
    tr.setAttribute("aria-selected", String(bankSel === key));
    tr.addEventListener("click", () => selectBank(key));
    tr.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        selectBank(key);
      }
    });
    tr.append(...cells);
    return tr;
  };

  const rows = order.map((bank, idx) => {
    const e = perBank.get(bank)!;
    let status = "";
    let cls = "";
    if (writing && progress) {
      if (idx < activeIdx) [status, cls] = ["done", "st-done"];
      else if (idx === activeIdx) [status, cls] = [`writing ${progress.bank_done}/${progress.bank_total}`, "st-active"];
      else [status, cls] = ["pending", "st-pending"];
    }
    return row(
      bankKey(bank),
      [cell(bankLabel(bank), "dim"), cell(String(e.files)), cell(formatBytes(e.bytes), "dim"), cell(status, cls)],
      cls === "st-active" ? "bank-active" : "",
    );
  });
  // With several banks, an "All" row gets back to the full list.
  if (order.length > 1) {
    rows.unshift(row("all", [cell("All", "dim"), cell(String(plan.files.length)), cell(formatBytes(plan.total_bytes), "dim"), cell("")]));
  }
  body.replaceChildren(...rows);
}

function renderFiles() {
  const title = $("#b-files-title");
  const body = $("#b-files-body");
  if (!plan) {
    title.textContent = "";
    body.replaceChildren();
    return;
  }
  const files = bankSel === "all" ? plan.files : plan.files.filter((f) => bankKey(f.bank) === bankSel);
  const bytes = files.reduce((n, f) => n + f.size_bytes, 0);
  title.textContent =
    (bankSel === "all" ? "All banks" : bankSel === "root" ? "Card root" : `Bank ${bankSel.padStart(2, "0")}`) +
    ` · ${files.length} file${files.length === 1 ? "" : "s"} · ${formatBytes(bytes)}`;
  body.replaceChildren(
    ...files.map((f) => {
      const tr = document.createElement("tr");
      tr.append(cell(bankLabel(f.bank), "dim"), cell(f.dest), cell(f.src.replace(sourceDir ?? "", "").replace(/^\//, "")));
      return tr;
    }),
  );
}

function renderPlan() {
  $("#b-warnings").replaceChildren(
    ...(plan?.warnings ?? []).map((w) => Object.assign(document.createElement("li"), { textContent: w })),
  );
  // A new roll may not contain the bank that was selected.
  if (!plan || (bankSel !== "all" && !plan.files.some((f) => bankKey(f.bank) === bankSel))) bankSel = "all";
  renderBankTable();
  renderFiles();
}

function updateBuildPanel() {
  const banks = banksValue();
  $("#b-banks-out").textContent = String(banks);

  const perBank = perBankValue();

  const pool = poolFiles();
  if (plan && poolKey() !== planPoolKey) {
    plan = null;
    renderPlan();
  }

  const wanted = perBank === null ? null : Math.min(banks * perBank, MAX_FILES_TOTAL);
  $("#b-pool").textContent = !sourceScan
    ? "Choose a source folder in step 2 to draw samples from."
    : `${pool.length} eligible file${pool.length === 1 ? "" : "s"}` +
      (wanted === null ? "" : ` / ${Math.min(wanted, pool.length)} used`);

  const vol = selectedVolume();
  const cardOk = !!vol && vol.warnings.length === 0;
  ($("#b-roll") as HTMLButtonElement).disabled = writing || formatting || pool.length === 0 || perBank === null;
  ($("#b-write") as HTMLButtonElement).disabled = writing || formatting || !plan || !cardOk;

  // Say why Write is unavailable instead of leaving a silently dimmed button.
  let reason = "";
  if (!sourceScan) reason = "Choose a source folder in step 2";
  else if (pool.length === 0) reason = "No eligible files in the pool";
  else if (!plan) reason = "Press Roll to pick a random selection";
  else if (!vol) reason = "Select a card in step 1";
  else if (!cardOk) reason = "The selected card is not usable";
  ($("#b-write") as HTMLButtonElement).title = reason;
  updateTabs();
  ($("#b-cancel") as HTMLElement).hidden = !writing;
  updateFormatPanel();

  if (!writing && !$("#b-status").dataset.sticky) {
    $("#b-status").textContent = plan
      ? `${plan.files.length} files · ${formatBytes(plan.total_bytes)} · seed ${plan.seed}` +
        (cardOk ? ` · ready to write to ${vol!.name || vol!.mount_point}` : ` · ${reason.toLowerCase()}`)
      : sourceScan && pool.length > 0
        ? `${reason}, then Write to card.`
        : "";
  }
}

function setBuildStatus(text: string, sticky = false) {
  const el = $("#b-status");
  el.textContent = text;
  if (sticky) el.dataset.sticky = "1";
  else delete el.dataset.sticky;
}

async function roll() {
  lastWriteOk = false;
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

/** Recent (time, bytes) samples used to estimate speed over a sliding window. */
const speedSamples: { t: number; bytes: number }[] = [];
const SPEED_WINDOW_MS = 6000;

function formatDuration(secs: number): string {
  if (!isFinite(secs) || secs < 0) return "…";
  if (secs < 60) return `${Math.max(1, Math.round(secs))}s`;
  const m = Math.round(secs / 60);
  return m < 60 ? `${m} min` : `${Math.floor(m / 60)}h ${m % 60}m`;
}

function onWriteProgress(p: WriteProgress) {
  progress = p;
  const now = performance.now();
  speedSamples.push({ t: now, bytes: p.bytes_done });
  while (speedSamples.length > 2 && now - speedSamples[0].t > SPEED_WINDOW_MS) speedSamples.shift();

  ($("#b-bar") as HTMLElement).style.width = `${p.bytes_total ? (100 * p.bytes_done) / p.bytes_total : 0}%`;
  renderBankTable();

  if (p.phase === "deleting") {
    setBuildStatus(`Deleting existing audio… ${p.done_files + 1}/${p.total_files} · ${p.current}`, true);
    return;
  }
  if (p.phase === "formatting") {
    setBuildStatus("Formatting the card to clear it…", true);
    return;
  }
  const first = speedSamples[0];
  const dt = (now - first.t) / 1000;
  const speed = dt > 0.5 ? (p.bytes_done - first.bytes) / dt : 0;
  const eta = speed > 0 ? (p.bytes_total - p.bytes_done) / speed : NaN;
  setBuildStatus(
    [
      `Bank ${bankLabel(p.bank)}`,
      `file ${Math.min(p.done_files + 1, p.total_files)}/${p.total_files}`,
      `${formatBytes(p.bytes_done)} / ${formatBytes(p.bytes_total)}`,
      speed > 0 ? `${(speed / 1e6).toFixed(1)} MB/s` : null,
      speed > 0 ? `~${formatDuration(eta)} left` : null,
    ]
      .filter(Boolean)
      .join(" · "),
    true,
  );
}

async function writePlan() {
  const vol = selectedVolume();
  if (!plan || !vol || writing) return;
  const label = vol.name || vol.mount_point;
  const existing = $<HTMLSelectElement>("#b-existing").value as Existing;

  // Ask the backend exactly what this would do before anything is touched.
  let preview: ChangePreview;
  try {
    preview = await invoke<ChangePreview>("preview_card_changes", { cardPath: vol.mount_point, plan, existing });
  } catch (err) {
    setBuildStatus(`Error: ${err}`, true);
    return;
  }
  if (preview.copy_files === 0 && preview.delete_files === 0) {
    setBuildStatus(`${label} already matches this selection. Nothing to do.`, true);
    return;
  }

  const files = (n: number) => `${n} file${n === 1 ? "" : "s"}`;
  const copyText = `copy ${files(preview.copy_files)} (${formatBytes(preview.copy_bytes)})`;
  const skipText = preview.unchanged_files > 0 ? `, skipping ${files(preview.unchanged_files)} already on the card` : "";
  const notes = preview.warnings.length > 0 ? `\n\nNote: ${preview.warnings.join(" ")}` : "";
  const message =
    preview.will_format
      ? `This will FORMAT ${label}, erasing EVERYTHING on it (including any non-audio files), then ${copyText}. Continue?${notes}`
      : preview.delete_files > 0
      ? `This will DELETE ${files(preview.delete_files)} of audio (${formatBytes(preview.delete_bytes)}) from ${label}, then ${copyText}. Other files on the card are not touched. Continue?${notes}`
      : existing === "add"
        ? `Add ${files(preview.copy_files)} (${formatBytes(preview.copy_bytes)}) to ${label}${skipText}? Audio already on the card is kept.${notes}`
        : `Copy ${files(preview.copy_files)} (${formatBytes(preview.copy_bytes)}) to ${label}? Existing files on the card are not touched.${notes}`;

  let go: boolean;
  try {
    go = await ask(message, { title: "Write to card", kind: preview.delete_files > 0 || preview.will_format ? "warning" : "info" });
  } catch (err) {
    setBuildStatus(`Could not show the confirmation dialog: ${err}`, true);
    return;
  }
  if (!go) {
    setBuildStatus("Write cancelled.", true);
    return;
  }

  writing = true;
  updateBuildPanel();
  progress = null;
  speedSamples.length = 0;
  ($("#b-progress") as HTMLElement).hidden = false;
  const unlisten = await listen<WriteProgress>("write-progress", (e) => onWriteProgress(e.payload));
  try {
    const n = await invoke<number>("write_card", { cardPath: vol.mount_point, plan, existing });
    lastWriteOk = true;
    const extras = [
      preview.delete_files > 0 ? `deleted ${preview.delete_files}` : null,
      preview.unchanged_files > 0 ? `skipped ${preview.unchanged_files} already there` : null,
    ].filter(Boolean);
    setBuildStatus(
      `Wrote ${n} file${n === 1 ? "" : "s"} (${formatBytes(preview.copy_bytes)}) to ${label}${extras.length ? ` · ${extras.join(" · ")}` : ""}. Eject the card before unplugging it.`,
      true,
    );
    plan = null;
    renderPlan();
    // Formatting can remount the card under a new path, so re-read the volume list first.
    await refreshVolumes();
    await scanSelected();
  } catch (err) {
    setBuildStatus(`Error: ${err}`, true);
  } finally {
    unlisten();
    writing = false;
    progress = null;
    ($("#b-progress") as HTMLElement).hidden = true;
    ($("#b-bar") as HTMLElement).style.width = "0";
    renderBankTable();
    updateBuildPanel();
  }
}

// ---- Format card ------------------------------------------------------------

let formatting = false;
const LABEL_RE = /^[A-Z0-9_-]{1,11}$/;

function labelValue(): string {
  // FAT labels are upper-case; normalise as the user types.
  const el = $<HTMLInputElement>("#fmt-label");
  const cleaned = el.value.toUpperCase().replace(/[^A-Z0-9_-]/g, "");
  if (cleaned !== el.value) el.value = cleaned;
  el.setAttribute("aria-invalid", String(!LABEL_RE.test(cleaned)));
  return cleaned;
}

function confirmName(vol: Volume): string {
  return vol.name || "ERASE";
}

function updateFormatPanel() {
  const vol = selectedVolume();
  const label = labelValue();
  const openBtn = $("#fmt-open") as HTMLButtonElement;

  $("#fmt-info").textContent = !vol
    ? "No card selected"
    : vol.formattable
      ? `Erases the whole disk and creates one ${vol.format_fs ?? "FAT32"} partition with an MBR boot record.`
      : `Cannot format: ${vol.format_blocker ?? "unsupported"}`;
  openBtn.disabled = formatting || writing || !vol || !vol.formattable || !LABEL_RE.test(label);

  const typed = $<HTMLInputElement>("#fmt-typed").value.trim();
  ($("#fmt-go") as HTMLButtonElement).disabled = formatting || !vol || typed !== confirmName(vol);
  if (!vol || formatting) return;
}

function closeFormatConfirm() {
  $("#fmt-confirm").hidden = true;
  $<HTMLInputElement>("#fmt-typed").value = "";
  updateFormatPanel();
}

function openFormatConfirm() {
  const vol = selectedVolume();
  if (!vol) return;
  const disk = vol.disk_size_bytes ? `${formatBytes(vol.disk_size_bytes)} disk` : "the whole disk";
  const parts = vol.partition_count ? `, including all ${vol.partition_count} partition${vol.partition_count === 1 ? "" : "s"}` : "";
  $("#fmt-warning").textContent =
    `This will ERASE EVERYTHING on ${vol.name || vol.mount_point} (${disk}${parts}) and create one ` +
    `${vol.format_fs ?? "FAT32"} partition named ${labelValue()}. This cannot be undone. ` +
    `Type "${confirmName(vol)}" to confirm.`;
  $<HTMLInputElement>("#fmt-typed").value = "";
  $("#fmt-confirm").hidden = false;
  $<HTMLInputElement>("#fmt-typed").focus();
  updateFormatPanel();
}

async function formatSelected() {
  const vol = selectedVolume();
  if (!vol || formatting || $<HTMLInputElement>("#fmt-typed").value.trim() !== confirmName(vol)) return;
  const label = labelValue();
  if (!LABEL_RE.test(label)) return;

  formatting = true;
  plan = null;
  renderPlan();
  closeFormatConfirm();
  setBuildStatus(`Formatting ${vol.name || vol.mount_point}… do not remove the card.`, true);
  updateBuildPanel();
  try {
    const newMount = await invoke<string>("format_card", { mountPoint: vol.mount_point, label });
    setBuildStatus(`Formatted as ${label}.`, true);
    // The volume remounts under its new name; wait for it to show up, then select it.
    for (let i = 0; i < 10; i++) {
      await refreshVolumes();
      if (volumes.some((v) => v.mount_point === newMount)) {
        select(newMount);
        break;
      }
      await new Promise((r) => setTimeout(r, 1000));
    }
  } catch (err) {
    setBuildStatus(`Format failed: ${err}`, true);
  } finally {
    formatting = false;
    updateBuildPanel();
  }
}

// ---- Workflow tabs ----------------------------------------------------------

type Step = "card" | "files" | "build";
const STEPS: Step[] = ["card", "files", "build"];

function showStep(step: Step, focus = false) {
  for (const s of STEPS) {
    const on = s === step;
    const tab = $(`#tab-${s}`);
    tab.setAttribute("aria-selected", String(on));
    tab.tabIndex = on ? 0 : -1;
    $(`#panel-${s}`).hidden = !on;
  }
  if (focus) $(`#tab-${step}`).focus();
  if (step === "build") updateBuildPanel();
}

function setStepState(step: Step, meta: string, state: "todo" | "done" | "warn") {
  $(`#meta-${step}`).textContent = meta;
  $(`#dot-${step}`).dataset.state = state;
  $(`#tab-${step}`).classList.toggle("done", state === "done");
}

/** Summarise each step on its tab so the workflow's progress is visible from anywhere. */
function updateTabs() {
  const vol = selectedVolume();
  setStepState(
    "card",
    vol ? `${vol.name || vol.mount_point} · ${vol.file_system || "?"}` : "No card",
    !vol ? "todo" : vol.warnings.length > 0 ? "warn" : "done",
  );

  const pool = poolFiles().length;
  setStepState("files", sourceScan ? `${pool} eligible file${pool === 1 ? "" : "s"}` : "No folder", pool > 0 ? "done" : "todo");

  setStepState(
    "build",
    lastWriteOk ? "Card written" : plan ? `${plan.files.length} files rolled` : "Not built",
    lastWriteOk ? "done" : "todo",
  );
}

function wireTabs() {
  STEPS.forEach((s, idx) => {
    const tab = $(`#tab-${s}`);
    tab.addEventListener("click", () => showStep(s));
    tab.addEventListener("keydown", (e: KeyboardEvent) => {
      const next = e.key === "ArrowRight" ? idx + 1 : e.key === "ArrowLeft" ? idx - 1 : e.key === "Home" ? 0 : e.key === "End" ? STEPS.length - 1 : null;
      if (next === null) return;
      e.preventDefault();
      showStep(STEPS[(next + STEPS.length) % STEPS.length], true);
    });
  });
  document.querySelectorAll<HTMLElement>("[data-goto]").forEach((b) =>
    b.addEventListener("click", () => showStep(b.dataset.goto as Step, true)),
  );
}

window.addEventListener("DOMContentLoaded", () => {
  wireTabs();
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
    delete $("#b-status").dataset.sticky;
    updateBuildPanel();
  });
  $("#fmt-label").addEventListener("input", updateFormatPanel);
  $("#fmt-open").addEventListener("click", openFormatConfirm);
  $("#fmt-typed").addEventListener("input", updateFormatPanel);
  $("#fmt-cancel").addEventListener("click", closeFormatConfirm);
  $("#fmt-go").addEventListener("click", () => void formatSelected());
  try {
    const saved = localStorage.getItem(EXISTING_KEY);
    if (saved === "refuse" || saved === "replace" || saved === "add") $<HTMLSelectElement>("#b-existing").value = saved;
  } catch {
    // storage unavailable; the mode just won't be remembered
  }
  $("#b-existing").addEventListener("change", () => {
    try {
      localStorage.setItem(EXISTING_KEY, $<HTMLSelectElement>("#b-existing").value);
    } catch {
      // ignore
    }
  });
  $("#b-roll").addEventListener("click", () => void roll());
  $("#b-write").addEventListener("click", () => void writePlan());
  $("#b-cancel").addEventListener("click", () => {
    setBuildStatus("Cancelling after the current chunk…", true);
    void invoke("cancel_write");
  });
  updateBuildPanel();

  void refreshVolumes();
  // Don't poll a card while it is being written or formatted: it only adds to the load on a slow device.
  setInterval(() => {
    if (!writing && !formatting) void refreshVolumes();
  }, POLL_MS);
});
