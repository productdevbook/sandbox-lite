import { createComponent, render, addAttribute, spreadAttributes } from "/__sl/astro.js";

async function meta(src) {
  const v = await src;
  if (!v) return { src: "" };
  if (typeof v === "string") return { src: v };
  if (v.default) return meta(v.default);
  return v;
}

export const Image = createComponent(async (result, props) => {
  const { src, alt = "", width, height, widths, sizes, densities, quality, format, inferSize, layout, fit, position, priority, loading = priority ? "eager" : "lazy", decoding = "async", ...rest } = props;
  const m = await meta(src);
  const w = width ?? m.width;
  const h = height ?? m.height;
  return render`<img${addAttribute(m.src, "src")}${addAttribute(alt, "alt")}${w ? addAttribute(w, "width") : ""}${h ? addAttribute(h, "height") : ""}${addAttribute(loading, "loading")}${addAttribute(decoding, "decoding")}${sizes ? addAttribute(sizes, "sizes") : ""}${spreadAttributes(rest)}>`;
}, "astro:assets/Image");

export const Picture = Image;
export const Font = createComponent(() => render``, "astro:assets/Font");

export async function getImage(options) {
  const m = await meta(options.src);
  return { src: m.src, srcSet: { values: [], attribute: "" }, rawOptions: options, options, attributes: { width: options.width ?? m.width, height: options.height ?? m.height, loading: "lazy", decoding: "async" } };
}

export async function inferRemoteSize() { return { width: 0, height: 0, format: "unknown" }; }
export const imageConfig = { service: { entrypoint: "sandbox-lite" } };
export function getConfiguredImageService() { return {}; }
export const isLocalService = () => false;
