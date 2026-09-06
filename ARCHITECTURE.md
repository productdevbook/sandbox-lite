# Architecture

sandbox-lite is one process: an axum server on one listener, a tokio runtime
with two worker threads, and compilation on the blocking pool. It never renders
HTML. It serves a shell page and compiled ES modules; the visitor's browser
renders the page with Astro's own runtime. This document follows a request
through the code; file paths are given so each claim can be checked.

## Host-based dispatch

`http::app` (`src/http/mod.rs`) builds two routers and a fallback that picks
one per request from the `Host` header (lowercased, port stripped):

- `tenant_from_host(host, domain)` returns a tenant id when the host is
  exactly `<label>.<domain>` and the label passes `valid_id`. The request goes
  to the **tenant router** with the id in its extensions:
  `/__sl/m/{*path}`, `/__sl/raw/{*path}`, `/__sl/routes.json`,
  `/__sl/renderers.json`, `/__sl/events`, `/__sl/check`,
  `/__sl/content/{name}`, `/__sl/shim/{name}`, `/__sl/astro.js`,
  `/__sl/shell.js`, `/__sl/live.js`, `/__sl/missing.js`, and a fallback
  (`preview::page`) for everything else. `require_preview_token` wraps all of
  it.
- Any other host goes to the **editor/API router**: `/` (the editor page),
  `/health`, `/api/stats`, `/api/bases`, `/api/tenants`, `/api/tenants/{id}`,
  `/api/t/{id}/files`, `/api/t/{id}/file/{*path}`, `/api/t/{id}/events`,
  `/api/t/{id}/check`, `/api/t/{id}/chat`, with a 64 MiB body limit.
  `require_api_token` wraps all of it.

`SECURITY.md` describes the two middlewares.

## Store: bases, overlays, versions (`src/store.rs`)

A **base** is a project directory read once at startup (`Base::load`): every
regular file, skipping `node_modules`, `.git`, `dist`, `.astro`, `.vercel`,
`.netlify`, `.output`. Files up to 256 KiB are held in memory
(`FileData::Mem`); larger ones stay on disk and are re-read on each access
(`FileData::Disk`). Bases come from every sub-directory of `--bases` (default
`./examples` when it exists) and from each `--base NAME=PATH`.

A **tenant** is an `Arc<Base>` plus an overlay,
`BTreeMap<String, Option<FileData>>`: `Some` is a file the tenant added or
changed, `None` is a tombstone hiding a base file. `Tenant::data` consults the
overlay first, so a tombstone makes the base file disappear; `Tenant::list`
merges both views and flags overlay entries as `modified`. Two tenants on the
same base share the base's memory; a tenant costs its overlay.

Each tenant has a **version**, a `u64` of milliseconds since the epoch at
creation. `write` and `delete` call `bump`, which sets it to
`max(now, previous + 1)` — strictly increasing — and sends
`{"type":"update"|"delete","path":…,"version":N}` on the tenant's
`tokio::sync::broadcast` channel (64 slots). Both SSE endpoints
(`/api/t/{id}/events`, `/__sl/events`) subscribe to that channel and open with
an `event: hello` carrying the current version.

`write_many` is the batch form used by an import: it settles the whole
resulting overlay under one lock — the removals, the tombstones and every file
— checks the quota against that result before writing anything, and bumps once.
Its event carries an empty path, which the editor reads as "the whole tree
changed" and the preview treats like any other update.

With a `--data-dir`, the overlay is persisted as
`<data-dir>/<id>/tenant.json` (`{"base": name}`), `<data-dir>/<id>/files/<path>`
and `<data-dir>/<id>/deleted.json` (the tombstones). Every write goes to disk;
files over 256 KiB are then kept only there. `Store::restore` rebuilds tenants
at startup with a fresh version and skips any whose base is not loaded.
`--no-persist` keeps everything in memory.

### Export and import (`src/http/archive.rs`)

`GET /api/t/{id}/export` writes a gzipped tar of the tenant: by default the
merged tree (`Tenant::list` plus `Tenant::data`, so a tombstoned base file is
absent), with `?overlay=1` only the overlay's own files plus
`.sandbox-lite/deleted.json` holding the tombstoned paths. Headers are
deterministic (mode 644, mtime 0) and a `FileData::Disk` entry is streamed from
its open handle rather than read into memory.

`POST /api/tenants/{id}/import` reads one back. Entries that are not regular
files are skipped (directories, pax headers) or refused (links, devices); an
entry name must be tenant-relative — a leading `/`, a `..` segment, a backslash
or a NUL byte is refused, while `./` and `//` are normalised away; the sizes are
summed from the tar headers and the import is refused with `413` before any
body is decompressed once they pass the quota. `.sandbox-lite/deleted.json` is
applied as tombstones instead of being written, so an overlay export imported
into a tenant on the same base reproduces the source overlay exactly.
`?replace=1` also drops the edits the archive does not carry — an edit over a
base file goes back to the base copy, which is what makes that reproduction
exact.

## Rendering a page

`GET http://acme.<domain>/blog/hello-world`, step by step:

1. **Shell.** No route matches, so `preview::page` (`src/http/preview.rs`)
   runs. It percent-decodes the path, and if `public/<path>` exists in the
   tenant it serves that file. Otherwise it answers `assets/shell.html` with
   `%TENANT%`, `%VERSION%` (the tenant's version), `%ASSETS%` (a hash of
   `astro.js`, `shell.js`, `live.js`) and `%ENV%` filled in, `Cache-Control:
   no-store`. `%ENV%` is `import.meta.env`: `DEV`, `PROD`, `MODE`, `SSR`,
   `BASE_URL`, `SITE` (from `sandbox-lite.json`), and the `PUBLIC_*` lines of
   the tenant's `.env`.
2. **Scripts.** The shell loads `/__sl/live.js` and `/__sl/shell.js`
   (`no-cache`, ETag = the assets hash). `live.js` opens an `EventSource` on
   `/__sl/events`.
3. **Route table.** `shell.js` fetches `/__sl/routes.json?v=<version>`.
   `routes::build` (`src/routes.rs`) turns every file under `src/pages/` into
   a route: `index` is dropped, files with a `_`-prefixed segment are skipped,
   `[param]` becomes `([^/]+?)`, `[...rest]` becomes `(.*?)` (optional when it
   is the whole segment), and the list is sorted by a per-segment key — 0 for
   static, 1 for `[param]`, 2 for `[...rest]` — compared lexicographically,
   so static routes come before dynamic ones and a route sorts before any
   longer route it is a prefix of; equal keys are ordered by path. The kind is
   `astro`, `md`, `mdx` or `endpoint`; the shell renders the first three and
   shows an "unsupported route" page for endpoints.
4. **Page module.** The shell imports `/__sl/astro.js` and
   `/__sl/m/src/pages/blog/[slug].astro?v=<version>`. `preview::module`
   cleans the path, reads the kind from the query, and on the blocking pool
   builds a `Resolver` (which reads `tsconfig.json` `paths`,
   `sandbox-lite.json` `imports` and `package.json` dependencies) and calls
   `Engine::serve`. The response is `Cache-Control: public, max-age=31536000,
   immutable` when the query has `v=`, `no-cache` otherwise.
5. **Module graph.** The compiled page imports its layout and components as
   `/__sl/m/…?v=N` URLs; each `<style>` block as
   `/__sl/m/<file>.astro?astro&type=style&index=i&v=N`, each hoisted
   `<script>` as `…type=script…`; `.css`/`.scss` files as modules that
   register their CSS; `astro:*` as `/__sl/shim/astro-*.js`; bare specifiers
   as CDN URLs; images as `{ src, width, height, format }` modules;
   unresolvable specifiers as `/__sl/missing.js?spec=…&from=…`, which throws.
   The browser follows the graph; each request repeats step 4.
6. **Static paths.** If the route has params and the module exports
   `getStaticPaths`, the shell calls it with its own `paginate`, keeps the
   entry whose params equal the matched ones, and uses its `props`.
7. **Render.** `experimental_AstroContainer.create({ resolve, astroConfig })`
   — `resolve` maps ids the runtime asks for (island component paths,
   `astro:*`) to daemon URLs. `/__sl/renderers.json` lists framework renderers
   (`resolve::renderers`: `@astrojs/react` and `@astrojs/preact` from
   `package.json`, or `sandbox-lite.json` `renderers`); each server renderer
   is imported and registered, each client entrypoint recorded. Astro's own
   `astro:jsx` renderer (`/__sl/shim/astro-jsx-runtime.js`, the JSX runtime
   plus `@astrojs/mdx/server.js`) is registered last; it renders MDX content.
   `container.renderToResponse(mod.default, { request, params, props })`
   produces the HTML.
8. **Document.** A `3xx` with `Location` becomes `location.replace`.
   Otherwise every CSS string that modules registered in
   `globalThis.__sl_css` becomes a `<style data-sl=key>` (with
   `type="text/tailwindcss"` when it contains `@import "tailwindcss"` or
   `@tailwind`), `live.js` is appended, the block is injected before
   `</head>`, and `document.write` replaces the shell. If Tailwind was seen,
   its browser build is loaded from jsDelivr.
9. **Errors.** Any exception fetches `/__sl/check` and renders an error page
   with the daemon's diagnostics; a missing route lists the routes. Error
   pages also load `live.js`, so they reload when the file is fixed.

The editor (`assets/editor.html`) is the other client: it embeds the preview
in an `<iframe>` and edits files through `/api/t/{id}/file/{path}`.

## The transform engine (`src/transform/mod.rs`)

`Engine::build(tenant, path, kind)` returns an `Arc<Built>`; `Engine::serve`
turns one into response text. The `kind` comes from the query string:
`Module` (default), `Style(i)` and `Script(i)` (`?astro&type=…&index=i`),
`Raw` (`?raw`), `Url` (`?url`).

What `compile` does per kind and extension:

| Input | Output |
|---|---|
| `.astro` | `astro_codegen` (`src/transform/astro.rs`): oxc parses, `<style lang="scss\|sass">` blocks are pre-compiled with grass, the compiler emits a module importing helpers from `/__sl/astro.js`; the CSS of each `<style>` and each hoisted `<script>` are kept aside; relative `client:component-path` values are made root-absolute |
| `.ts` `.tsx` `.jsx` `.mts` | oxc transform (`src/transform/js.rs`): TypeScript stripped, JSX compiled |
| `.js` `.mjs` | served as-is when it parses as ESM, otherwise transformed |
| `.css` | a module that registers the text in `globalThis.__sl_css`, with relative `url()` and `@import` targets rewritten to `/__sl/raw/…` (`src/transform/css.rs`) |
| `.scss` `.sass` | grass over the tenant's file tree (`src/transform/scss.rs`), then as `.css` |
| `.md` | frontmatter + pulldown-cmark HTML, wrapped as a page component that renders through `layout:` when set (`src/transform/markdown.rs`) |
| `.mdx` | satteri-mdxjs (`src/transform/mdx.rs`): frontmatter split off, headings given ids and collected, JSX compiled against `astro/jsx-runtime`, then wrapped as `@astrojs/mdx` does — `frontmatter`, `file`, `url`, `getHeadings`, a `layout:` wrapper, and a default `Content` export tagged for the `astro:jsx` renderer |
| `.json` | `export default JSON.parse(…)` |
| images | `export default { src: "/__sl/raw/…", width, height, format, fsPath }` |
| `Style(i)` / `Script(i)` | builds the `.astro` module (cached) and returns its i-th CSS block or script as its own module |
| `Raw` / `Url` | the text as a string export / the `/__sl/raw/` URL as a string export |
| anything else | the `/__sl/raw/` URL as a string export |

A `Built` holds the compiled `body`, plus what `serve` needs later: `specs`
(byte ranges of every import specifier, from oxc's module record), `globs`
(byte ranges and options of every `import.meta.glob(...)` call,
`src/transform/glob.rs`), the CSS blocks, the hoisted scripts, warnings and
an island count.

### The cache key

`build` hashes (xxh3-128) the following, NUL-separated, and uses the result
as the cache key:

1. the kind tag — `m`, `s<i>`, `j<i>`, `r`, `u`;
2. the path;
3. `site` from the tenant's `sandbox-lite.json`, or empty — the compiler
   bakes it into the output (`astro_global_args`);
4. only for `.scss`/`.sass` files and `.astro` files whose source contains
   `lang="scss"` or `lang="sass"`: the Sass fingerprint (below);
5. the file's bytes.

Not in the key: the tenant id, the version, the CDN, `package.json`,
`tsconfig.json`, and the tenant's other files except through the fingerprint.
So the cache is content-addressed: two tenants with byte-identical files share
one entry, and a changed file simply hashes to a new key — nothing is
invalidated explicitly.

The cache (`Engine::cache`) is a `HashMap<u128, Arc<Built>>` behind a mutex
with a `VecDeque` of insertion order. The budget (`--cache-mb`, default 64
MiB) counts `body` bytes; when exceeded, the oldest-inserted entries are
dropped (FIFO, not LRU). `/api/stats` reports entries, bytes, hits and misses.

### Why resolution and `import.meta.glob` happen at serve time

The compiled body contains the specifiers as written — `./Header`,
`@/data/products`, `react` — and `import.meta.glob("./posts/*.md")` calls as
written. What they resolve to is a property of the tenant, not of the file:

- `./Header` is `Header.astro` in one tenant and `Header.tsx` in another
  (`Resolver::probe` tries extensions, then `index` files);
- the set of files matching `./posts/*.md` is the tenant's file list;
- every emitted URL must carry the tenant's current `?v=`.

None of that can live in a content-addressed entry, so `Engine::serve` does it
on every request: it resolves each `spec` with the `Resolver`, expands each
glob against `tenant.list()` (`glob::expand` writes hoisted
`import * as __sl_glob<i>_<j> from "/__sl/m/<file>?v=N"` statements for
`eager: true`, or `() => import(...)` thunks otherwise, keyed relative to the
importer or absolute when the pattern is), prepends
`import.meta.env = globalThis.__sl_env || {};` when the module needs it, and
splices the replacements into the body by byte range. The compile is cached;
the splice is cheap and never is.

### Resolution order (`src/resolve.rs`)

For a specifier in `importer`, `Resolver::resolve` returns the first of:

1. the specifier itself when it is external (`http://`, `https://`, `//`,
   `data:`, `blob:`, `/__sl/`);
2. `/__sl/shim/astro-<name>.js` for `astro:<name>`, and a shim for
   `astro/components`, `astro/zod`, `astro/config`, `astro/loaders`,
   `astro/types`, `astro/jsx-runtime`;
3. for `./`, `../`, `/`-rooted and `tsconfig` `paths` specifiers: the first
   existing file among the candidate plus `""`, `.ts`, `.tsx`, `.js`, `.jsx`,
   `.mjs`, `.mts`, `.json`, `.astro`, `.md`, `.mdx`, then `/index.*` — as
   `/__sl/m/<path>?<query>&v=N`; or `/__sl/missing.js?spec=…&from=…` when
   nothing exists;
4. a `sandbox-lite.json` `imports` entry (exact key, or prefix when the key
   ends with `/`);
5. the CDN: `<cdn>/<name>@<range><subpath>` when `package.json` lists the
   package with a plain version range, else `<cdn>/<specifier>`.

The shim modules under `assets/shims/` go through the same resolver when
served (`preview::shim`), so a shim can import `astro:*` or a bare package.

### The Sass fingerprint

`scss::fingerprint` XORs a hash of the path and content of every `.scss` and
`.sass` file in the tenant. Because a Sass module's output depends on the
partials it `@use`s, and those are other files, the fingerprint is part of the
cache key of every Sass-consuming module: editing any Sass file changes the
key of all of them. Coarse, and correct.

## Versions, browser cache and SSE

The version does three jobs:

- **Cache busting.** Every module URL the daemon emits carries
  `?v=<version at serve time>`, and `preview::module` marks such responses
  `immutable`. After an edit the version is higher, every URL is new, and the
  browser fetches fresh; the old URLs stay in its cache harmlessly. The daemon
  does not read the value of `v` — it only checks that the parameter is
  present — and always serves the file's current content.
- **Reload.** `live.js` reloads the page on any `update`/`delete` event, and
  on the `hello` event when the daemon's version is higher than the one baked
  into the shell — which covers an edit that landed between the shell and the
  `EventSource` connecting, and a daemon restart, since restored tenants get a
  fresh version.
- **Editor refresh.** The editor subscribes to `/api/t/{id}/events` to reload
  its file list and the open file.

The transform cache never sees the version; it is content-addressed.

## Content collections and Markdown (`src/transform/content.rs`)

`/__sl/content/<name>` returns `{entries, dates}`. An entry is `.md`/
`.markdown` with parsed YAML frontmatter as `data`, the body, the rendered
HTML and headings; `.mdx` with `data` and the body only; `.json` arrays as one
entry per item, other JSON as one entry; `.yaml`/`.yml` as one entry. The
`astro:content` shim fetches that JSON and implements `getCollection`,
`getEntry` and `render` on top of it; for an `.mdx` entry `render` imports the
compiled module at `/__sl/m/<filePath>` and returns its `Content`,
`getHeadings()` and `frontmatter`.

Which files those are comes from `src/content.config.ts` (or the Astro 2–4
`src/content/config.ts`), parsed with oxc and never executed. `parse_config`
maps `export const collections = { name: … }` to the `defineCollection({…})`
literal behind each name and reads three things out of it: a
`glob({ pattern, base })` loader — patterns matched with `globset` under
`base`, negations included, ids being the base-relative path without its
extension, lower-cased; a `file(path)` loader — one JSON or YAML file whose
top level is an array of objects with an `id` or an object keyed by id; and
the `z.date()` / `z.coerce.date()` fields of a `schema: z.object({…})`
literal, which become the `dates` list the shim turns into `Date` objects.
Anything the reader cannot see through — a computed pattern, a custom loader,
a schema built by a function — leaves that part unset, and the collection
falls back to `src/content/<name>/` with the shim's ISO-shaped-string guess
for dates.

## `check` (`src/check.rs`)

`sandbox-lite check DIR…` loads each directory as a base, creates an
in-memory tenant on it, and calls `Engine::build(…, Kind::Module)` on every
source file under `src/` (`.astro`, `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`,
`.mts`, `.md`, `.mdx`). It prints diagnostics and a census — islands, glob calls,
Sass, MDX, endpoints, `@astrojs/*` integrations, bare imports — and exits 1 on
compile errors, 2 when a directory cannot be loaded. `/api/t/{id}/check` and
`/__sl/check` run the same loop (`api::check_tenant`) on a live tenant; the
error page uses the latter.

## Chat (`src/http/ai.rs`)

`POST /api/t/{id}/chat` runs a tool loop against the Anthropic Messages API
with five tools scoped to the tenant — `list_files`, `read_file`,
`write_file`, `delete_file`, `check_site` — for at most 16 rounds, and returns
the model's last text, the changed paths and the tenant version. Writes go
through the same `Tenant::write` as the editor, so previews follow.

## Layout

```
src/main.rs            flags, base loading, restore, listener
src/store.rs           Base, Tenant (overlay, version, events, persistence), Store, clean_path, valid_id
src/http/mod.rs        routers, host dispatch, the two token middlewares, MIME table
src/http/api.rs        editor page, /api handlers, SSE, check_tenant
src/http/archive.rs    tar.gz export and import of a tenant tree
src/http/preview.rs    tenant-host handlers: shell, modules, raw, routes, renderers, shims, content
src/http/ai.rs         chat tool loop
src/resolve.rs         Resolver, CDN URLs, renderers, sandbox-lite.json, tsconfig paths
src/routes.rs          src/pages → route table
src/transform/mod.rs   Engine, cache, Built, serve-time splice
src/transform/astro.rs astro_codegen wrapper
src/transform/js.rs    oxc transform and import scanning
src/transform/css.rs   CSS-as-module, relative URL rewriting
src/transform/scss.rs  grass over the tenant tree, fingerprint
src/transform/glob.rs  import.meta.glob detection and expansion
src/transform/markdown.rs, content.rs   Markdown pages and collections
src/transform/mdx.rs   MDX pages and entries through satteri-mdxjs
src/check.rs           the check subcommand
assets/shell.html, shell.js, live.js    the browser side of a render
assets/editor.html     the editor
assets/astro.js        Astro runtime + container, built by scripts/build-runtime.sh
assets/astro-jsx.js    Astro's JSX runtime + astro:jsx renderer, built by the same script
assets/shims/          browser stand-ins for astro:* modules and the React/Preact renderers
```
