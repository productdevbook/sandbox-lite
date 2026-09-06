(() => {
  // headless Chrome's virtual clock never advances while a fetch is open, so a screenshot run gets no live connection
  if (new URLSearchParams(location.search).has("sl_shot")) return;
  const prev = window.__sl_es;
  if (prev && prev.readyState !== EventSource.CLOSED) return;
  const es = new EventSource("/__sl/events");
  window.__sl_es = es;
  es.addEventListener("hello", (e) => {
    const v = Number(e.data);
    if (window.__sl && window.__sl.version && v > window.__sl.version) location.reload();
  });
  es.onmessage = (e) => {
    let d;
    try { d = JSON.parse(e.data); } catch { return; }
    if (d.type === "update" || d.type === "delete") location.reload();
  };
})();
