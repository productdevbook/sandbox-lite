const sl = window.__sl;
const V = sl.version;
const registry = (globalThis.__sl_css ||= new Map());
const TAILWIND_CDN = "https://cdn.jsdelivr.net/npm/@tailwindcss/browser@4";

function esc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}

async function fetchJSON(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url} answered ${r.status}`);
  return r.json();
}

function matchRoute(routes, pathname) {
  for (const r of routes) {
    const m = new RegExp(r.pattern).exec(pathname);
    if (!m) continue;
    const params = {};
    r.params.forEach((p, i) => {
      const v = m[i + 1];
      params[p] = v === undefined || v === "" ? undefined : decodeURIComponent(v);
    });
    return { route: r, params };
  }
  return null;
}

function sameParams(a, b) {
  const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
  for (const k of keys) {
    const x = a[k] === undefined ? "" : String(a[k]);
    const y = b[k] === undefined ? "" : String(b[k]);
    if (x !== y) return false;
  }
  return true;
}

function paginate(route) {
  return (data, opts = {}) => {
    const size = opts.pageSize ?? 10;
    const last = Math.max(1, Math.ceil(data.length / size));
    const pageParam = route.params.includes("page") ? "page" : route.params[route.params.length - 1];
    const base = route.route.replace(/\/\[\.\.\.[^\]]+\]$|\/\[[^\]]+\]$/, "");
    const urlFor = (n) => (n === 1 && route.route.includes("[...") ? base || "/" : `${base}/${n}`);
    const out = [];
    for (let n = 1; n <= last; n++) {
      const start = (n - 1) * size;
      const slice = data.slice(start, start + size);
      out.push({
        params: { ...(opts.params || {}), [pageParam]: n === 1 && route.route.includes("[...") ? undefined : String(n) },
        props: {
          ...(opts.props || {}),
          page: {
            data: slice, start, end: start + slice.length - 1, size, total: data.length, currentPage: n, lastPage: last,
            url: { current: urlFor(n), prev: n > 1 ? urlFor(n - 1) : undefined, next: n < last ? urlFor(n + 1) : undefined, first: urlFor(1), last: urlFor(last) },
          },
        },
      });
    }
    return out;
  };
}

function resolveId(id) {
  if (/^(https?:)?\/\//.test(id) || id.startsWith("/__sl/")) return id;
  if (id.startsWith("astro:")) return `/__sl/shim/astro-${id.slice(6).replace(/\.js$/, "").replace(/\//g, "-")}.js`;
  const clean = id.startsWith("/") ? id.slice(1) : id;
  return `/__sl/m/${clean}${clean.includes("?") ? "&" : "?"}v=${V}`;
}

async function addRenderers(container) {
  const renderers = await fetchJSON(`/__sl/renderers.json?v=${V}`);
  for (const r of renderers) {
    const mod = await import(r.server);
    container.addServerRenderer({ name: r.name, renderer: mod.default });
    if (r.client) container.addClientRenderer({ name: r.name, entrypoint: r.client });
  }
}

function headExtras() {
  const blocks = [];
  let tailwind = false;
  for (const [key, css] of registry) {
    const tw = /@import\s+["']tailwindcss|@tailwind\s/.test(css);
    tailwind ||= tw;
    blocks.push(`<style${tw ? ' type="text/tailwindcss"' : ""} data-sl="${esc(key)}">\n${css}\n</style>`);
  }
  blocks.push(`<script type="module" src="/__sl/live.js"></script>`);
  return { html: blocks.join("\n"), tailwind };
}

function injectHead(html, extras) {
  const i = html.search(/<\/head\s*>/i);
  if (i >= 0) return html.slice(0, i) + extras + "\n" + html.slice(i);
  const b = html.search(/<body[\s>]/i);
  if (b >= 0) return html.slice(0, b) + `<head>${extras}</head>` + html.slice(b);
  return `<!doctype html><html><head><meta charset="utf-8">${extras}</head><body>${html}</body></html>`;
}

function replaceDocument(html) {
  document.open();
  document.write(html);
  document.close();
}

function showError({ title, message, stack, diagnostics = [], routes }) {
  const diag = diagnostics
    .map((d) => `<li><b>${esc(d.file)}${d.line ? `:${d.line}:${d.column}` : ""}</b> — ${esc(d.text)}${d.hint ? `<br><i>${esc(d.hint)}</i>` : ""}</li>`)
    .join("");
  const list = routes ? `<p>Known routes:</p><ul>${routes.map((r) => `<li><a href="${esc(r.route.replace(/\[[^\]]+\]/g, "…"))}">${esc(r.route)}</a> → ${esc(r.component)}</li>`).join("")}</ul>` : "";
  replaceDocument(`<!doctype html><html><head><meta charset="utf-8"><title>${esc(title)}</title>
<style>body{margin:0;background:#1a1b26;color:#c0caf5;font:14px/1.5 ui-monospace,Menlo,monospace;padding:32px}
h1{color:#f7768e;font-size:18px;margin:0 0 12px}pre{white-space:pre-wrap;background:#16161e;padding:12px;border-radius:6px;overflow:auto}
ul{padding-left:18px}b{color:#7aa2f7}i{color:#9ece6a}a{color:#7dcfff}small{color:#565f89}</style>
<script type="module" src="/__sl/live.js"></script></head>
<body><h1>${esc(title)}</h1><pre>${esc(message || "")}</pre>${diag ? `<ul>${diag}</ul>` : ""}${stack ? `<pre>${esc(stack)}</pre>` : ""}${list}
<small>sandbox-lite · tenant ${esc(sl.tenant)} · the page reloads itself when a file changes</small></body></html>`);
}

async function main() {
  const t0 = performance.now();
  const routes = await fetchJSON(`/__sl/routes.json?v=${V}`);
  const hit = matchRoute(routes, location.pathname);
  if (!hit) return showError({ title: "404 — no matching page", message: `Nothing in src/pages matches ${location.pathname}`, routes });
  if (hit.route.kind !== "astro" && hit.route.kind !== "md") {
    return showError({ title: "Unsupported route", message: `${hit.route.component} is a ${hit.route.kind} route. sandbox-lite renders .astro and .md pages in the browser; endpoints and MDX are not available in the preview.` });
  }
  const astro = await import("/__sl/astro.js");
  const mod = await import(`/__sl/m/${hit.route.component}?v=${V}`);
  if (typeof mod.default !== "function") throw new Error(`${hit.route.component} has no default export component`);
  let props = {};
  let params = hit.params;
  if (hit.route.params.length && typeof mod.getStaticPaths === "function") {
    const paths = await mod.getStaticPaths({ paginate: paginate(hit.route) });
    const entry = (Array.isArray(paths) ? paths : []).find((p) => sameParams(hit.params, p.params || {}));
    if (!entry) return showError({ title: "404 — no static path", message: `getStaticPaths() in ${hit.route.component} does not return params ${JSON.stringify(hit.params)}` });
    props = entry.props || {};
    params = entry.params;
  }
  const container = await astro.experimental_AstroContainer.create({ resolve: resolveId, astroConfig: sl.env.SITE ? { site: sl.env.SITE } : undefined });
  await addRenderers(container);
  const res = await container.renderToResponse(mod.default, {
    request: new Request(location.href),
    params,
    props,
    partial: false,
    routeType: "page",
  });
  const location_ = res.headers.get("location");
  if (res.status >= 300 && res.status < 400 && location_) return location.replace(location_);
  const html = await res.text();
  if (res.status >= 400 && !html.trim()) return showError({ title: `${res.status}`, message: `The page responded with status ${res.status} and no body` });
  const extras = headExtras();
  replaceDocument(injectHead(html, extras.html));
  if (extras.tailwind) {
    const s = document.createElement("script");
    s.src = TAILWIND_CDN;
    document.head.appendChild(s);
  }
  console.debug(`[sandbox-lite] ${hit.route.component} rendered in ${(performance.now() - t0).toFixed(0)} ms`);
}

main().catch(async (e) => {
  let diagnostics = [];
  try { diagnostics = (await fetchJSON("/__sl/check")).diagnostics.filter((d) => d.severity === "error"); } catch {}
  console.error(e);
  showError({ title: "Render failed", message: e && e.message ? e.message : String(e), stack: e && e.stack, diagnostics });
});
