import { readFileSync } from "node:fs";
import { relative } from "node:path";
import { fileURLToPath } from "node:url";

const file = process.argv[2] || fileURLToPath(new URL("../assets/editor.html", import.meta.url));
const name = ((r) => (r && !r.startsWith("..") ? r : file))(relative(process.cwd(), file));
const html = readFileSync(file, "utf8");
const SINK = /\.(innerHTML|outerHTML)\s*\+?=(?!=)|\.insertAdjacentHTML\s*\(/g;

function mask(js) {
  const out = js.split("");
  const literals = [];
  const blank = (a, b) => { for (let k = a; k < b; k++) if (out[k] !== "\n") out[k] = " "; };
  const regexStart = (i) => {
    let k = i - 1;
    while (k >= 0 && /\s/.test(out[k])) k--;
    return k < 0 || /[(,=:[!&|?{};+\-*%<>~^]/.test(out[k]) || /(^|[^\w$])(return|typeof|case|in|of|delete|void|throw|new|do|else)$/.test(js.slice(0, k + 1));
  };
  const string = (i, q) => {
    for (let j = i + 1; j < js.length; j++) {
      if (js[j] === "\\") j++;
      else if (js[j] === q) return j + 1;
      else if (js[j] === "\n") return j;
    }
    return js.length;
  };
  const regex = (i) => {
    let cls = false;
    for (let j = i + 1; j < js.length; j++) {
      const c = js[j];
      if (c === "\\") j++;
      else if (c === "\n") return j;
      else if (cls) cls = c !== "]";
      else if (c === "[") cls = true;
      else if (c === "/") { j++; while (/[a-z]/i.test(js[j] || "")) j++; return j; }
    }
    return js.length;
  };
  const template = (i) => {
    let j = i + 1;
    while (j < js.length && js[j] !== "`") {
      if (js[j] === "$" && js[j + 1] === "{") { j = code(j + 2, true); continue; }
      const n = js[j] === "\\" ? 2 : 1;
      blank(j, j + n);
      j += n;
    }
    return j + 1;
  };
  const code = (i, interpolation) => {
    let depth = 0;
    while (i < js.length) {
      const c = js[i], n = js[i + 1];
      if (c === "/" && n === "/") { const j = js.indexOf("\n", i); const e = j < 0 ? js.length : j; blank(i, e); i = e; }
      else if (c === "/" && n === "*") { const j = js.indexOf("*/", i + 2); const e = j < 0 ? js.length : j + 2; blank(i, e); i = e; }
      else if (c === '"' || c === "'") { const e = string(i, c); literals.push([i, e]); blank(i + 1, e - 1); i = e; }
      else if (c === "`") { const e = template(i); literals.push([i, e]); i = e; }
      else if (c === "/" && regexStart(i)) { const e = regex(i); blank(i + 1, e); i = e; }
      else if (c === "{") { depth++; i++; }
      else if (c === "}") { if (interpolation && depth === 0) return i + 1; depth--; i++; }
      else i++;
    }
    return i;
  };
  code(0, false);
  return { masked: out.join(""), literals };
}

const matchClose = (text, open, a, b) => {
  let depth = 0;
  for (let k = open; k < text.length; k++) {
    if (text[k] === a) depth++;
    else if (text[k] === b && --depth === 0) return k;
  }
  return text.length;
};

const statementEnd = (text, i) => {
  let depth = 0;
  for (let k = i; k < text.length; k++) {
    const c = text[k];
    if ("([{".includes(c)) depth++;
    else if (")]}".includes(c)) { if (depth === 0) return k; depth--; }
    else if (c === ";" && depth === 0) return k;
  }
  return text.length;
};

const findings = [];
let sinks = 0, scripts = 0;
for (const m of html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/g)) {
  scripts++;
  const js = m[1], offset = m.index + m[0].indexOf(js);
  const { masked, literals } = mask(js);
  let depth = 0;
  for (const c of masked) { if ("([{".includes(c)) depth++; else if (")]}".includes(c)) depth--; }
  if (depth !== 0) { console.error(`${name}: could not tokenise the script (brackets do not balance once literals are masked)`); process.exit(2); }
  for (const s of masked.matchAll(SINK)) {
    sinks++;
    const start = s.index, from = start + s[0].length, end = statementEnd(masked, from);
    const where = `${name}:${html.slice(0, offset + start).split("\n").length}`;
    const sink = s[0].trim();
    const outside = masked.slice(from, end).split("");
    for (const it of masked.slice(from, end).matchAll(/\$\{/g)) {
      const open = from + it.index + 1, close = matchClose(masked, open, "{", "}");
      const inner = masked.slice(open + 1, close);
      const lead = inner.match(/^\s*esc\s*\(/);
      const wrapped = lead && /^\s*$/.test(inner.slice(matchClose(inner, lead[0].length - 1, "(", ")") + 1));
      if (!wrapped) findings.push(`${where}: \${${js.slice(open + 1, close).trim()}} reaches ${sink} without esc()`);
      for (let k = it.index; k <= close - from; k++) outside[k] = " ";
    }
    if (outside.includes("+")) findings.push(`${where}: string concatenation into ${sink}; use one template literal with every value in esc()`);
    if (!literals.some(([a]) => a >= from && a < end)) findings.push(`${where}: ${sink} takes a non-literal expression; build the markup from a template literal with every value in esc()`);
  }
}

if (!scripts) { console.error(`${name}: no inline <script> found; the check only covers script blocks in this file`); process.exit(2); }
const def = html.match(/^[ \t]*const esc = .*$/m);
if (sinks && !def) findings.push(`${name}: HTML sinks exist but esc() is not defined`);
if (def) {
  let esc;
  try { esc = new Function(`${def[0]}\nreturn esc;`)(); } catch (e) { findings.push(`${name}: cannot evaluate the esc() definition: ${e.message}`); }
  const hostile = `<img src=x onerror=alert(1)>.astro & "q" 'a'`;
  const escaped = "&lt;img src=x onerror=alert(1)&gt;.astro &amp; &quot;q&quot; &#39;a&#39;";
  if (esc && (esc(hostile) !== escaped || esc(7) !== "7")) findings.push(`${name}: esc() does not escape & < > " '`);
}

if (findings.length) { console.error(findings.join("\n")); process.exit(1); }
console.log(`editor-escaping: ${name}: ${sinks} HTML sink${sinks === 1 ? "" : "s"}, every interpolation goes through esc()`);
