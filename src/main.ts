import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask, open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { closePreview, currentPreviewPath, initPreview, openPreview, refreshLevels, syncDock, trimLabel, trims, trimsSignature, type NormalizeChoice } from "./preview";
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
  void loadCardSettings();
  if (mount) void scanSelected();
}

let refreshingVolumes = false;

async function refreshVolumes() {
  // A slow or wedged card can make one refresh take a while; don't stack more behind it.
  if (refreshingVolumes) return;
  refreshingVolumes = true;
  try {
    await refreshVolumesOnce();
  } finally {
    refreshingVolumes = false;
  }
}

async function refreshVolumesOnce() {
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
        tr.className = `pick${f.eligible ? "" : " ineligible"}${currentPreviewPath() === f.path ? " previewing" : ""}`;
        tr.addEventListener("click", () => {
          void openPreview(f.path, f.relative_path);
          renderSource();
        });
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
  trim?: { start: number; end: number } | null;
  render?: boolean;
}

interface Plan {
  seed: number;
  banks: number;
  files: PlannedFile[];
  total_bytes: number;
  warnings: string[];
  normalize?: "peak" | "rms" | null;
  capacity_bytes?: number | null;
  requested_files?: number;
  requested_bytes?: number;
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

/**
 * Bytes a plan may use on the selected card: the whole card when erasing (it gets formatted), the
 * free space otherwise. A small margin covers file-system overhead. `null` when no card is chosen.
 */
function usableCapacity(): number | null {
  const vol = selectedVolume();
  if (!vol) return null;
  const raw = $<HTMLSelectElement>("#b-existing").value === "replace" ? vol.total_bytes : vol.available_bytes;
  return Math.floor(raw * 0.995);
}

/** Whether the rolled plan fits the card, with the sentence to show about it. */
function fitState(): { over: boolean; text: string } {
  if (!plan) return { over: false, text: "" };
  const cap = usableCapacity();
  const size = formatBytes(plan.total_bytes);
  if (cap === null) return { over: false, text: `${size} · select a card to check it fits` };

  if (plan.total_bytes > cap) {
    const hint = $<HTMLInputElement>("#b-fit").checked
      ? "Roll again to fit it"
      : "Tick Fit to card, lower the banks or files per bank, or tighten the filters";
    const note = $<HTMLSelectElement>("#b-existing").value === "add" ? " (files already on the card are skipped, so the real size may be smaller)" : "";
    return {
      over: true,
      text: `TOO BIG: ${size} won't fit. The card holds ${formatBytes(cap)}, so this is over by ${formatBytes(plan.total_bytes - cap)}. ${hint}.${note}`,
    };
  }
  const trimmed =
    plan.requested_files && plan.requested_files > plan.files.length
      ? ` · fit to card: ${plan.files.length} of the ${plan.requested_files} files you asked for`
      : "";
  return { over: false, text: `${size} of ${formatBytes(cap)} (${Math.round((100 * plan.total_bytes) / cap)}%)${trimmed}` };
}

function normalizeValue(): NormalizeChoice {
  return $<HTMLSelectElement>("#b-normalize").value as NormalizeChoice;
}

/** What a plan was built from: any change to the pool, the trims or the normalize mode makes it stale. */
function poolKey(): string {
  return (
    poolFiles()
      .map((f) => f.path)
      .join("\n") + `|${trimsSignature()}|${normalizeValue()}|fit:${$<HTMLInputElement>("#b-fit").checked}`
  );
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
      const notes = [f.trim ? `trimmed ${trimLabel(f.trim)}` : null, f.render && plan?.normalize ? "normalized" : null].filter(Boolean);
      tr.className = "pick";
      tr.addEventListener("click", () => void openPreview(f.src, f.dest));
      tr.append(
        cell(bankLabel(f.bank), "dim"),
        cell(notes.length ? `${f.dest} · ${notes.join(", ")}` : f.dest),
        cell(f.src.replace(sourceDir ?? "", "").replace(/^\//, "")),
      );
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
  if (settingsOnly) {
    updateSettingsOnlyPanel();
    return;
  }
  ($("#b-write") as HTMLButtonElement).textContent = "Write to card";
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
    ? "Choose a source folder in step 3 to draw samples from."
    : `${pool.length} eligible file${pool.length === 1 ? "" : "s"}` +
      (wanted === null ? "" : ` / ${Math.min(wanted, pool.length)} used`);

  const vol = selectedVolume();
  const cardOk = !!vol && vol.warnings.length === 0;
  ($("#b-roll") as HTMLButtonElement).disabled = writing || formatting || pool.length === 0 || perBank === null;
  const fits = fitState();
  ($("#b-write") as HTMLButtonElement).disabled = writing || formatting || !plan || !cardOk || fits.over;

  // Say why Write is unavailable instead of leaving a silently dimmed button.
  let reason = "";
  if (!sourceScan) reason = "Choose a source folder in step 3";
  else if (pool.length === 0) reason = "No eligible files in the pool";
  else if (!plan) reason = "Press Roll to pick a random selection";
  else if (!vol) reason = "Select a card in step 1";
  else if (!cardOk) reason = "The selected card is not usable";
  else if (fits.over) reason = "The selection is too big for the card";
  ($("#b-write") as HTMLButtonElement).title = reason;
  updateTabs();
  ($("#b-cancel") as HTMLElement).hidden = !writing;
  updateFormatPanel();

  const status = $("#b-status");
  const showing = !writing && !status.dataset.sticky;
  status.classList.toggle("status-bad", showing && fits.over);
  if (showing) {
    status.textContent = plan
      ? fits.over
        ? `${plan.files.length} files · ${fits.text}`
        : `${plan.files.length} files · ${fits.text} · seed ${plan.seed}` +
          (cardOk ? ` · ready to write to ${vol!.name || vol!.mount_point}` : ` · ${reason.toLowerCase()}`)
      : sourceScan && pool.length > 0
        ? `${reason}, then Write to card.`
        : "";
  }
}

function setBuildStatus(text: string, sticky = false) {
  const el = $("#b-status");
  el.classList.remove("status-bad");
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
      candidates: pool.map((f) => ({ path: f.path, size_bytes: f.size_bytes, trim: trims.get(f.path) ?? null })),
      banks: banksValue(),
      filesPerBank: perBank,
      seed: crypto.getRandomValues(new Uint32Array(1))[0],
      cardFileSystem: selectedVolume()?.file_system ?? null,
      normalize: normalizeValue() === "off" ? null : normalizeValue(),
      capacityBytes: usableCapacity(),
      fit: $<HTMLInputElement>("#b-fit").checked,
    });
    planPoolKey = poolKey();
  } catch (err) {
    plan = null;
    setBuildStatus(`Error: ${err}`, true);
  }
  renderPlan();
  updateBuildPanel();
}

/**
 * Speed and ETA come from the whole run so far, not a short window. A slow card accepts data in
 * bursts with 10-30 s stalls in between, and a short window would read 0 MB/s during every stall.
 */
let writeStartedAt = 0; // when the first byte of this write was reported
let lastAdvanceAt = 0; // last time bytes_done increased
let lastBytes = 0;
const STALL_AFTER_MS = 3000;

function resetWriteStats() {
  writeStartedAt = 0;
  lastAdvanceAt = 0;
  lastBytes = 0;
}

function formatDuration(secs: number): string {
  if (!isFinite(secs) || secs < 0) return "…";
  if (secs < 60) return `${Math.max(1, Math.round(secs))}s`;
  const m = Math.round(secs / 60);
  return m < 60 ? `${m} min` : `${Math.floor(m / 60)}h ${m % 60}m`;
}

function onWriteProgress(p: WriteProgress) {
  progress = p;
  const now = performance.now();

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

  if (writeStartedAt === 0) {
    writeStartedAt = now;
    lastAdvanceAt = now;
  }
  if (p.bytes_done > lastBytes) {
    lastBytes = p.bytes_done;
    lastAdvanceAt = now;
  }
  const elapsed = (now - writeStartedAt) / 1000;
  const stalledFor = now - lastAdvanceAt;
  // Wait for a few seconds of data before trusting an average.
  const avg = elapsed >= 3 && p.bytes_done > 0 ? p.bytes_done / elapsed : 0;
  const remaining = p.bytes_total - p.bytes_done;

  setBuildStatus(
    [
      `Bank ${bankLabel(p.bank)}`,
      `file ${Math.min(p.done_files + 1, p.total_files)}/${p.total_files}`,
      `${formatBytes(p.bytes_done)} / ${formatBytes(p.bytes_total)}`,
      avg > 0 ? `avg ${(avg / 1e6).toFixed(1)} MB/s` : "measuring speed…",
      avg > 0 ? `~${formatDuration(remaining / avg)} left` : null,
      `${formatDuration(elapsed)} elapsed`,
      stalledFor >= STALL_AFTER_MS ? `waiting on card ${Math.round(stalledFor / 1000)}s` : null,
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
  resetWriteStats();
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
    let settingsNote = "";
    if ($<HTMLInputElement>("#b-settings").checked && selectedVolume()) {
      try {
        await writeSettingsToCard();
        settingsNote = " · settings.txt written";
      } catch (err) {
        settingsNote = ` · settings.txt NOT written (${err})`;
      }
    }
    if (settingsNote) $("#b-status").textContent += settingsNote;
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

// ---- Settings ---------------------------------------------------------------

type SettingKind =
  | { type: "choice"; options: { value: number; label: string }[] }
  | { type: "toggle" }
  | { type: "number"; min: number; max: number; unit: string };
type SettingDef = { key: string; label: string; group: string; help: string; default: number; common: boolean } & SettingKind;

interface CardSettings {
  exists: boolean;
  values: Record<string, number>;
  unknown: string[];
  warnings: string[];
}

let settingsSchema: SettingDef[] = [];
let settingsDraft: Record<string, number> = {}; // current value of every setting
let settingsBaseline: Record<string, number> = {}; // as last loaded or saved, to tell what was edited
let settingsInFile = new Set<string>(); // settings present in the card's settings.txt
let settingsFileExists = false;
let settingsDirty = false;
let settingsSaved = false;
let settingsIncludeTouched = false;

// "Skip (write settings only)": Build Card updates settings.txt and leaves the audio alone.
let settingsOnly = false;
const SKIP_KEY = "radiomusic-manager:settingsOnly";

function setSettingsOnly(on: boolean) {
  settingsOnly = on;
  $<HTMLInputElement>("#skip-files").checked = on;
  $<HTMLInputElement>("#b-skip").checked = on;
  $("#app").classList.toggle("settings-only", on);
  try {
    localStorage.setItem(SKIP_KEY, on ? "1" : "0");
  } catch {
    // storage unavailable; the choice just won't be remembered
  }
  delete $("#b-status").dataset.sticky;
  $("#b-status").textContent = "";
  updateBuildPanel();
  syncDock(currentStep, settingsOnly);
}

function updateSettingsOnlyPanel() {
  const vol = selectedVolume();
  const count = Object.keys(settingsToWrite()).length;
  const label = vol ? vol.name || vol.mount_point : "";
  const write = $("#b-write") as HTMLButtonElement;
  write.textContent = "Write settings";
  write.disabled = writing || formatting || !vol || settingsSchema.length === 0;
  write.title = !vol ? "Select a card in step 1" : "";
  ($("#b-cancel") as HTMLElement).hidden = true;
  $("#b-skip-note").textContent = !vol
    ? "Select a card in step 1, then write your settings to it."
    : `${count} setting${count === 1 ? "" : "s"} will be written to settings.txt on ${label}. No audio is touched, and nothing is formatted or deleted.`;
  updateTabs();
  updateFormatPanel();
}

async function writeSettingsOnly() {
  const vol = selectedVolume();
  if (!vol || writing || formatting) return;
  if (Object.keys(settingsToWrite()).length === 0) {
    setBuildStatus("Nothing to write: every setting is at its default and the card has no settings.txt.", true);
    return;
  }
  try {
    const n = await writeSettingsToCard();
    setBuildStatus(`Wrote ${n} setting${n === 1 ? "" : "s"} to settings.txt on ${vol.name || vol.mount_point}. Audio was not touched.`, true);
  } catch (err) {
    setBuildStatus(`Error: ${err}`, true);
  }
  updateBuildPanel();
}

function settingsDefaults(): Record<string, number> {
  return Object.fromEntries(settingsSchema.map((d) => [d.key, d.default]));
}

function recomputeSettingsDirty() {
  settingsDirty = settingsSchema.some((d) => settingsDraft[d.key] !== settingsBaseline[d.key]);
}

function settingRow(def: SettingDef): HTMLElement {
  const row = document.createElement("div");
  row.className = "set-row";
  row.dataset.key = def.key;

  const head = document.createElement("div");
  head.className = "set-head";
  const label = Object.assign(document.createElement("label"), { className: "set-label", textContent: def.label });
  const mod = Object.assign(document.createElement("span"), { className: "set-mod", title: "Different from the default" });
  const reset = Object.assign(document.createElement("button"), { type: "button", className: "set-reset", textContent: "default" });
  reset.title = "Back to the default";
  head.append(label, mod, reset);

  const control = document.createElement("div");
  control.className = "set-control";
  const id = `set-${def.key}`;
  label.htmlFor = id;

  const commit = (v: number) => {
    settingsDraft[def.key] = v;
    recomputeSettingsDirty();
    refreshSettingsUi();
  };

  if (def.type === "choice") {
    const sel = document.createElement("select");
    sel.id = id;
    for (const o of def.options) sel.append(new Option(`${o.label}${o.value === def.default ? "  (default)" : ""}`, String(o.value)));
    sel.addEventListener("change", () => commit(Number(sel.value)));
    control.append(sel);
  } else if (def.type === "toggle") {
    const box = document.createElement("input");
    box.type = "checkbox";
    box.id = id;
    box.addEventListener("change", () => commit(box.checked ? 1 : 0));
    control.append(box, Object.assign(document.createElement("span"), { className: "set-unit", textContent: def.default ? "on by default" : "off by default" }));
  } else {
    const input = document.createElement("input");
    input.type = "text";
    input.id = id;
    input.inputMode = "numeric";
    input.addEventListener("input", () => {
      const t = input.value.trim();
      const n = /^-?\d+$/.test(t) ? Number(t) : NaN;
      const ok = Number.isInteger(n) && n >= def.min && n <= def.max;
      input.setAttribute("aria-invalid", String(!ok));
      if (ok) commit(n);
    });
    control.append(
      input,
      Object.assign(document.createElement("span"), {
        className: "set-unit",
        textContent: `${def.unit ? def.unit + " · " : ""}${def.min} to ${def.max} · default ${def.default}`,
      }),
    );
  }

  reset.addEventListener("click", () => {
    settingsDraft[def.key] = def.default;
    recomputeSettingsDirty();
    refreshSettingsUi(true);
  });

  row.append(head, control, Object.assign(document.createElement("p"), { className: "set-help", textContent: def.help }));
  return row;
}

function renderSettingsForm() {
  const build = (defs: SettingDef[], into: HTMLElement) => {
    const groups = new Map<string, SettingDef[]>();
    for (const d of defs) groups.set(d.group, [...(groups.get(d.group) ?? []), d]);
    into.replaceChildren(
      ...[...groups.entries()].map(([name, items]) => {
        const fs = document.createElement("fieldset");
        fs.className = "set-group";
        fs.append(Object.assign(document.createElement("legend"), { textContent: name }), ...items.map(settingRow));
        return fs;
      }),
    );
  };
  build(settingsSchema.filter((d) => d.common), $("#settings-form"));
  build(settingsSchema.filter((d) => !d.common), $("#settings-form-more"));
}

/** Push the draft into the controls and update the row markers, buttons and status text. */
function refreshSettingsUi(syncControls = false) {
  for (const def of settingsSchema) {
    const row = document.querySelector<HTMLElement>(`.set-row[data-key="${def.key}"]`);
    if (!row) continue;
    const value = settingsDraft[def.key];
    row.classList.toggle("changed", value !== def.default);
    const el = row.querySelector<HTMLInputElement | HTMLSelectElement>(`#set-${def.key}`);
    if (el && (syncControls || document.activeElement !== el)) {
      if (def.type === "toggle") (el as HTMLInputElement).checked = value === 1;
      else {
        el.value = String(value);
        el.removeAttribute("aria-invalid");
      }
    }
  }

  const vol = selectedVolume();
  const label = vol ? vol.name || vol.mount_point : "";
  const dirtyNote = settingsDirty ? " · unsaved changes" : "";
  $("#settings-source").textContent =
    (!vol
      ? "No card selected. These are the Radio Music's defaults; pick a card to load its settings.txt."
      : settingsFileExists
        ? `settings.txt on ${label} (${settingsInFile.size} setting${settingsInFile.size === 1 ? "" : "s"} set in the file).`
        : `${label} has no settings.txt, so the Radio Music will use its defaults. Save to create one.`) + dirtyNote;

  const canTouchCard = !!vol && !writing && !formatting;
  ($("#settings-save") as HTMLButtonElement).disabled = !canTouchCard || (!settingsDirty && settingsFileExists);
  ($("#settings-load") as HTMLButtonElement).disabled = !canTouchCard || !settingsFileExists;

  // Keep "Write settings.txt" in the Build step sensible unless the user has chosen for themselves.
  const nonDefault = settingsSchema.some((d) => settingsDraft[d.key] !== d.default);
  if (!settingsIncludeTouched) $<HTMLInputElement>("#b-settings").checked = settingsFileExists || settingsDirty || nonDefault;
  updateTabs();
}

/** Read the selected card's settings.txt. Edits you haven't saved are never overwritten silently. */
async function loadCardSettings(force = false) {
  const vol = selectedVolume();
  // Before the settings list has arrived there is nothing to merge into; initSettings() loads again.
  if (settingsSchema.length === 0) return;
  if (!vol || writing || formatting) {
    refreshSettingsUi();
    return;
  }
  try {
    const found = await invoke<CardSettings>("read_card_settings", { cardPath: vol.mount_point });
    settingsFileExists = found.exists;
    settingsInFile = new Set(Object.keys(found.values));
    $("#settings-warnings").replaceChildren(
      ...[
        ...found.warnings,
        ...(found.unknown.length ? [`Left alone: ${found.unknown.join(", ")} (not settings this app knows about)`] : []),
      ].map((w) => Object.assign(document.createElement("li"), { textContent: w })),
    );
    if (found.exists && (force || !settingsDirty)) {
      settingsDraft = { ...settingsDefaults(), ...found.values };
      settingsBaseline = { ...settingsDraft };
      settingsDirty = false;
    } else if (!found.exists) {
      // A card without settings.txt: keep whatever has been set up here.
      settingsBaseline = settingsDirty ? settingsBaseline : { ...settingsDraft };
    }
  } catch (err) {
    $("#settings-source").textContent = `Could not read settings.txt: ${err}`;
    return;
  }
  refreshSettingsUi(true);
}

/** The settings that belong in the file: ones already there, plus anything changed from default. */
function settingsToWrite(): Record<string, number> {
  const out: Record<string, number> = {};
  for (const d of settingsSchema) {
    if (settingsInFile.has(d.key) || settingsDraft[d.key] !== d.default) out[d.key] = settingsDraft[d.key];
  }
  return out;
}

async function writeSettingsToCard(): Promise<number> {
  const vol = selectedVolume();
  if (!vol) throw new Error("no card selected");
  const values = settingsToWrite();
  await invoke("write_card_settings", { cardPath: vol.mount_point, values });
  for (const k of Object.keys(values)) settingsInFile.add(k);
  settingsFileExists = true;
  settingsBaseline = { ...settingsDraft };
  settingsDirty = false;
  settingsSaved = true;
  refreshSettingsUi();
  return Object.keys(values).length;
}

async function saveSettingsClicked() {
  try {
    const n = await writeSettingsToCard();
    $("#settings-warnings").replaceChildren(
      Object.assign(document.createElement("li"), { textContent: `Saved ${n} setting${n === 1 ? "" : "s"} to settings.txt.` }),
    );
  } catch (err) {
    $("#settings-warnings").replaceChildren(Object.assign(document.createElement("li"), { textContent: `Could not save: ${err}` }));
  }
}

async function initSettings() {
  try {
    settingsSchema = await invoke<SettingDef[]>("settings_schema");
  } catch (err) {
    $("#settings-source").textContent = `Could not load the settings list: ${err}`;
    return;
  }
  settingsDraft = settingsDefaults();
  settingsBaseline = { ...settingsDraft };
  renderSettingsForm();
  refreshSettingsUi(true);
  $("#settings-save").addEventListener("click", () => void saveSettingsClicked());
  $("#settings-load").addEventListener("click", () => void loadCardSettings(true));
  $("#settings-defaults").addEventListener("click", () => {
    settingsDraft = settingsDefaults();
    recomputeSettingsDirty();
    refreshSettingsUi(true);
  });
  $("#b-settings").addEventListener("change", () => (settingsIncludeTouched = true));
  // A card may already have been selected while the list was loading.
  void loadCardSettings(true);
}

// ---- Workflow tabs ----------------------------------------------------------

type Step = "card" | "settings" | "files" | "build";
const STEPS: Step[] = ["card", "settings", "files", "build"];

let currentStep: Step = "card";

function showStep(step: Step, focus = false) {
  // Leaving a tab ends any preview, so coming back finds the screen as you left it.
  if (step !== currentStep) closePreview();
  currentStep = step;
  for (const s of STEPS) {
    const on = s === step;
    const tab = $(`#tab-${s}`);
    tab.setAttribute("aria-selected", String(on));
    tab.tabIndex = on ? 0 : -1;
    $(`#panel-${s}`).hidden = !on;
  }
  if (focus) $(`#tab-${step}`).focus();
  if (step === "build") updateBuildPanel();
  syncDock(step, settingsOnly);
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

  setStepState(
    "settings",
    !selected ? "Defaults" : settingsDirty ? "Unsaved changes" : settingsFileExists ? "settings.txt loaded" : "No settings.txt",
    settingsDirty ? "warn" : settingsFileExists || settingsSaved ? "done" : "todo",
  );

  const pool = poolFiles().length;
  setStepState(
    "files",
    settingsOnly ? "Skipped" : sourceScan ? `${pool} eligible file${pool === 1 ? "" : "s"}` : "No folder",
    settingsOnly || pool > 0 ? "done" : "todo",
  );

  if (settingsOnly) {
    setStepState(
      "build",
      settingsDirty ? "Settings · unsaved" : settingsSaved ? "Settings written" : "Settings only",
      settingsSaved && !settingsDirty ? "done" : "todo",
    );
  } else {
    setStepState(
      "build",
      lastWriteOk ? "Card written" : plan ? `${plan.files.length} files rolled` : "Not built",
      lastWriteOk ? "done" : "todo",
    );
  }
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
  // The webview won't navigate to external sites; hand the link to the system browser.
  const ecotone = $<HTMLAnchorElement>("#ecotone-link");
  ecotone.addEventListener("click", (e) => {
    e.preventDefault();
    void openUrl(ecotone.getAttribute("href")!);
  });
  initPreview({
    onTrimChange: () => {
      renderSource();
      updateBuildPanel();
    },
    getNormalize: normalizeValue,
  });
  $("#b-normalize").addEventListener("change", () => {
    updateBuildPanel();
    void refreshLevels();
  });
  for (const id of ["#skip-files", "#b-skip"]) {
    $(id).addEventListener("change", (e) => setSettingsOnly((e.target as HTMLInputElement).checked));
  }
  try {
    if (localStorage.getItem(SKIP_KEY) === "1") setSettingsOnly(true);
  } catch {
    // storage unavailable
  }
  void initSettings();
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
  $("#b-fit").addEventListener("change", updateBuildPanel);
  $("#b-existing").addEventListener("change", () => {
    updateBuildPanel();
    try {
      localStorage.setItem(EXISTING_KEY, $<HTMLSelectElement>("#b-existing").value);
    } catch {
      // ignore
    }
  });
  $("#b-roll").addEventListener("click", () => void roll());
  $("#b-write").addEventListener("click", () => void (settingsOnly ? writeSettingsOnly() : writePlan()));
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
