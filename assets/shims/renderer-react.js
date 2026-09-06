import React from "react";
import { renderToString } from "react-dom/server";

const slotName = (s) => s.trim().replace(/[-_]([a-z])/g, (_, w) => w.toUpperCase());

function StaticHtml({ value, name, hydrate = true }) {
  if (!value) return null;
  const tag = hydrate ? "astro-slot" : "astro-static-slot";
  return React.createElement(tag, { name, suppressHydrationWarning: true, dangerouslySetInnerHTML: { __html: value } });
}

async function check(Component, props, children) {
  if (typeof Component === "object") return String(Component.$$typeof).startsWith("Symbol(react");
  if (typeof Component !== "function") return false;
  if (Component.prototype != null && typeof Component.prototype.render === "function") {
    return React.Component.isPrototypeOf(Component) || React.PureComponent.isPrototypeOf(Component);
  }
  let isReact = false;
  function Tester(...args) {
    try {
      const vnode = Component(...args);
      const t = vnode && vnode.$$typeof && String(vnode.$$typeof);
      if (t && (t === "Symbol(react.element)" || t === "Symbol(react.transitional.element)")) isReact = true;
    } catch {}
    return React.createElement("div");
  }
  await renderToStaticMarkup(Tester, props, { default: children });
  return isReact;
}

async function renderToStaticMarkup(Component, props, { default: children, ...slotted } = {}, metadata) {
  const hydrate = metadata?.astroStaticSlot ? !!metadata.hydrate : true;
  const slots = {};
  for (const [key, value] of Object.entries(slotted)) {
    slots[slotName(key)] = React.createElement(StaticHtml, { hydrate, value, name: key });
  }
  const { class: _drop, ...rest } = props;
  const el = React.createElement(Component, { ...rest, ...slots }, children != null ? React.createElement(StaticHtml, { hydrate, value: children }) : undefined);
  return { html: renderToString(el) };
}

export default { name: "@astrojs/react", check, renderToStaticMarkup, supportsAstroStaticSlot: true };
