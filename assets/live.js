(() => {
  // headless Chrome's virtual clock never advances while a fetch is open, so a screenshot run gets no live connection
  if (new URLSearchParams(location.search).has("sl_shot")) return;
  const prev = window.__sl_es;
  if (prev && prev.readyState !== EventSource.CLOSED) return;
  const es = new EventSource("/__sl/events");
  window.__sl_es = es;

  const reload = () => location.reload();
  const styles = () => Array.from(document.querySelectorAll("style[data-sl]"));
  // Tailwind's browser build compiles a type="text/tailwindcss" block when it loads and offers no rebuild hook.
  const tailwind = (el) => el.hasAttribute("type");
  const rawUrl = (path) => `/__sl/raw/${path}`;
  const quoteRe = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

  function put(el, css) {
    el.textContent = css;
    (globalThis.__sl_css ||= new Map()).set(el.dataset.sl, css);
  }

  async function fetchCss(el, url) {
    const mod = await import(url);
    if (typeof mod.default !== "string") throw new Error(`${url} exported no CSS`);
    put(el, mod.default);
  }

  // A `.css` module has a block of its own. One reached through another sheet's `@import` — the
  // shape `global.css` uses for `tokens.css` — has none, and a fresh query on its URL is what makes
  // the browser fetch it again.
  function cssJobs(path, version) {
    const fresh = `${rawUrl(path)}?v=${version}`;
    const stale = new RegExp(`${quoteRe(rawUrl(path))}(\\?v=\\d+)?(?=[)"'\\s;,]|$)`, "g");
    const jobs = [];
    for (const el of styles()) {
      const imported = el.textContent.replace(stale, fresh);
      const own = el.dataset.sl === path;
      if (!own && imported === el.textContent) continue;
      if (tailwind(el)) return null;
      jobs.push(own ? () => fetchCss(el, `/__sl/m/${path}?v=${version}`) : async () => put(el, imported));
    }
    return jobs;
  }

  // The `<style>` blocks of one component, keyed `<file>?<index>` as shell.js wrote them.
  function styleJobs(path, version) {
    const prefix = `${path}?`;
    const jobs = [];
    for (const el of styles()) {
      if (!el.dataset.sl.startsWith(prefix)) continue;
      const index = Number(el.dataset.sl.slice(prefix.length));
      if (!Number.isInteger(index) || index < 0 || tailwind(el)) return null;
      jobs.push(() => fetchCss(el, `/__sl/m/${path}?astro&type=style&index=${index}&lang.css&v=${version}`));
    }
    return jobs;
  }

  async function swap(d) {
    // The overlay is not the rendered page: only a reload can put the page back.
    if (!document.body || document.body.hasAttribute("data-sl-overlay")) return false;
    const jobs = d.kind === "css" ? cssJobs(d.path, d.version) : d.kind === "style" ? styleJobs(d.path, d.version) : null;
    if (!jobs || jobs.length === 0) return false;
    for (const job of jobs) await job();
    return true;
  }

  es.addEventListener("hello", (e) => {
    const v = Number(e.data);
    if (window.__sl && window.__sl.version && v > window.__sl.version) reload();
  });
  es.onmessage = (e) => {
    let d;
    try { d = JSON.parse(e.data); } catch { return; }
    if (d.type !== "update" && d.type !== "delete") return;
    if (d.type === "delete" || !d.path) return reload();
    swap(d).then((swapped) => {
      // the page now matches this version, so a reconnect's hello must not send it back
      if (swapped && window.__sl) window.__sl.version = d.version;
      else reload();
    }, reload);
  };
})();
