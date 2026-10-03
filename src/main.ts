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

window.addEventListener("DOMContentLoaded", () => {
  document.querySelector("#scan-form")?.addEventListener("submit", async (e) => {
    e.preventDefault();
    const path = (document.querySelector("#scan-path") as HTMLInputElement).value;
    try {
      render(await invoke<ScanResult>("scan_directory", { path }));
    } catch (err) {
      document.querySelector("#scan-summary")!.textContent = `Error: ${err}`;
    }
  });
});
