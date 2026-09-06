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

// The daemon read `z.date()` / `z.coerce.date()` out of the schema, so guessing by shape is not needed.
function reviveFields(data, paths) {
  for (const path of paths) {
    const keys = path.split(".");
    let obj = data;
    for (let i = 0; i < keys.length - 1 && obj; i++) obj = obj[keys[i]];
    const key = keys[keys.length - 1];
    const value = obj?.[key];
    if (typeof value === "string" || typeof value === "number") obj[key] = new Date(value);
  }
  return data;
}

function load(name) {
  if (!cache.has(name)) {
    cache.set(name, fetch(`/__sl/content/${encodeURIComponent(name)}?v=${globalThis.__sl?.version ?? ""}`).then(async (r) => {
      if (!r.ok) throw new Error(`content collection '${name}' is not available (${r.status})`);
      const { entries, dates } = await r.json();
      for (const e of entries) e.data = dates ? reviveFields(e.data, dates) : reviveDates(e.data);
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
  if (!entry?.rendered && entry?.filePath?.endsWith(".mdx")) {
    const mod = await import(`/__sl/m/${entry.filePath}?v=${globalThis.__sl?.version ?? ""}`);
    return { Content: mod.Content, headings: mod.getHeadings(), remarkPluginFrontmatter: mod.frontmatter };
  }
  const html = entry?.rendered?.html ?? "";
  const Content = createComponent(() => renderTemplate`${unescapeHTML(html)}`, `${entry?.collection}/${entry?.id}:Content`);
  return { Content, headings: entry?.rendered?.metadata?.headings ?? [], remarkPluginFrontmatter: entry?.data ?? {} };
}

export function defineCollection(config) { return config; }
export function reference(collection) { return z; }
export function defineLiveCollection(config) { return config; }
export { z };
