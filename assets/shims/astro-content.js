import { createComponent, render as renderTemplate, unescapeHTML } from "/__sl/astro.js";
import { z } from "/__sl/shim/zod.js";

const cache = new Map();

const ISO_DATE = /^\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?)?$/;

// YAML parses bare dates as timestamps; the daemon serialises them as strings, so revive them here.
function reviveDates(value) {
  if (typeof value === "string") return ISO_DATE.test(value) ? new Date(value) : value;
  if (Array.isArray(value)) return value.map(reviveDates);
  if (value && typeof value === "object") {
    for (const k of Object.keys(value)) value[k] = reviveDates(value[k]);
  }
  return value;
}

function load(name) {
  if (!cache.has(name)) {
    cache.set(name, fetch(`/__sl/content/${encodeURIComponent(name)}?v=${globalThis.__sl?.version ?? ""}`).then(async (r) => {
      if (!r.ok) throw new Error(`content collection '${name}' is not available (${r.status})`);
      const entries = await r.json();
      for (const e of entries) e.data = reviveDates(e.data);
      return entries;
    }));
  }
  return cache.get(name);
}

export async function getCollection(name, filter) {
  const entries = await load(name);
  return typeof filter === "function" ? entries.filter(filter) : entries;
}

export async function getEntry(a, b) {
  const [collection, id] = typeof a === "object" && a ? [a.collection, a.id ?? a.slug] : [a, b];
  const entries = await load(collection);
  return entries.find((e) => e.id === id || e.slug === id);
}

export const getEntryBySlug = getEntry;
export const getDataEntryById = getEntry;
export async function getEntries(refs) { return Promise.all(refs.map((r) => getEntry(r))); }
export async function getLiveCollection() { return { entries: [] }; }
export async function getLiveEntry() { return { entry: undefined }; }

export async function render(entry) {
  const html = entry?.rendered?.html ?? "";
  const Content = createComponent(() => renderTemplate`${unescapeHTML(html)}`, `${entry?.collection}/${entry?.id}:Content`);
  return { Content, headings: entry?.rendered?.metadata?.headings ?? [], remarkPluginFrontmatter: entry?.data ?? {} };
}

export function defineCollection(config) { return config; }
export function reference(collection) { return z; }
export function defineLiveCollection(config) { return config; }
export { z };
