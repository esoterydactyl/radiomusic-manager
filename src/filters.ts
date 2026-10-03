// Pure filtering/sorting for the source library. No DOM access, so it can be
// exercised directly (e.g. `node --experimental-strip-types`).

export interface Filterable {
  relative_path: string;
  format: string;
  duration_secs: number | null;
}

export interface Filters {
  search: string; // substring match on the relative path, case-insensitive
  minSecs: number | null;
  maxSecs: number | null;
  format: string; // "" = any
}

export const NO_FILTERS: Filters = { search: "", minSecs: null, maxSecs: null, format: "" };

export function isFiltering(f: Filters): boolean {
  return (
    f.search.trim() !== "" || f.minSecs !== null || f.maxSecs !== null || f.format !== ""
  );
}

/** Parses "90", "1.5" or "1:30" into seconds; blank or invalid gives null. */
export function parseDuration(text: string): number | null {
  const t = text.trim();
  if (t === "") return null;
  const parts = t.split(":");
  if (parts.length > 3 || parts.some((p) => p.trim() === "" || !/^\d+(\.\d+)?$/.test(p.trim()))) return null;
  return parts.reduce((total, p) => total * 60 + Number(p), 0);
}

export function matches(file: Filterable, f: Filters): boolean {
  if (f.format && file.format !== f.format) return false;

  if (f.minSecs !== null || f.maxSecs !== null) {
    // Files with unknown length can't satisfy a length bound.
    if (file.duration_secs === null) return false;
    if (f.minSecs !== null && file.duration_secs < f.minSecs) return false;
    if (f.maxSecs !== null && file.duration_secs > f.maxSecs) return false;
  }

  const needle = f.search.trim().toLowerCase();
  if (needle) {
    const haystack = file.relative_path.toLowerCase();
    // Every whitespace-separated term must match somewhere.
    if (!needle.split(/\s+/).every((term) => haystack.includes(term))) return false;
  }
  return true;
}

export type SortKey = "file" | "length";

export function compareBy(key: SortKey, dir: 1 | -1) {
  const text = (v: string) => v.toLowerCase();
  return (a: Filterable, b: Filterable): number => {
    let r: number;
    switch (key) {
      case "length": {
        // Unknown lengths always sort last.
        const x = a.duration_secs ?? Number.POSITIVE_INFINITY;
        const y = b.duration_secs ?? Number.POSITIVE_INFINITY;
        if (x === y) r = 0;
        else if (x === Number.POSITIVE_INFINITY) return 1;
        else if (y === Number.POSITIVE_INFINITY) return -1;
        else r = x - y;
        break;
      }
      case "file":
        r = text(a.relative_path).localeCompare(text(b.relative_path));
        break;
    }
    return r * dir;
  };
}
