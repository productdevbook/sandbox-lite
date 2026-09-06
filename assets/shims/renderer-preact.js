import { h, Component } from "preact";
import { renderToString } from "preact-render-to-string";

const slotName = (s) => s.trim().replace(/[-_]([a-z])/g, (_, w) => w.toUpperCase());

function StaticHtml({ value, name, hydrate = true }) {
  if (!value) return null;
  return h(hydrate ? "astro-slot" : "astro-static-slot", { name, dangerouslySetInnerHTML: { __html: value } });
}

async function check(Comp, props, children) {
  if (typeof Comp !== "function") return false;
  if (Comp.prototype != null && typeof Comp.prototype.render === "function") return Component.isPrototypeOf(Comp);
  try {
    const out = Comp({ ...props, children: children ? h(StaticHtml, { value: children }) : undefined });
    return out != null && typeof out === "object" && "type" in out && "props" in out;
  } catch {
    return false;
  }
}

async function renderToStaticMarkup(Comp, props, { default: children, ...slotted } = {}, metadata) {
  const hydrate = metadata?.astroStaticSlot ? !!metadata.hydrate : true;
  const slots = {};
  for (const [key, value] of Object.entries(slotted)) slots[slotName(key)] = h(StaticHtml, { hydrate, value, name: key });
  const { class: _drop, ...rest } = props;
  return { html: renderToString(h(Comp, { ...rest, ...slots }, children != null ? h(StaticHtml, { hydrate, value: children }) : null)) };
}

export default { name: "@astrojs/preact", check, renderToStaticMarkup, supportsAstroStaticSlot: true };
