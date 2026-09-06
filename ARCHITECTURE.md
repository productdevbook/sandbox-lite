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
  `/health`, `/metrics`, `/api/stats`, `/api/bases`,
  `/api/bases/{name}/reload`, `/api/tenants`, `/api/tenants/{id}`,
  `/api/tenants/{id}/import`, `/api/t/{id}/files`, `/api/t/{id}/export`,
  `/api/t/{id}/file/{*path}`, `/api/t/{id}/events`, `/api/t/{id}/check`,
  `/api/t/{id}/chat`, `/api/t/{id}/chats` and `/api/t/{id}/chats/{chat}`, with
  a 64 MiB body limit. `require_api_token` wraps all of it, so that whole list
  is the `--api-token` surface — `import` and `export` included, which move
  whole file trees.

`SECURITY.md` describes the two middlewares.

## Store: bases, overlays, versions (`src/store.rs`)

A **base** is a project directory read at startup (`Base::load`): every regular
file, skipping `node_modules`, `.git`, `dist`, `.astro`, `.vercel`, `.netlify`,
`.output`. Files up to 256 KiB are held in memory (`FileData::Mem`); larger ones
stay on disk and are re-read on each access (`FileData::Disk`). Bases come from
every sub-directory of `--bases` (default `./examples` when it exists) that is
a directory in its own right — `Store::bases_in_dir` reads the entry's own
type, so a symbolic link is skipped rather than followed — from each
`--base NAME=PATH`, and from `POST /api/bases` at run time. All three put the
root through `Store::base_root` first, so with `--bases` set a root outside it
is refused whichever way it arrives. `Base::load`
also records a `stamp`: a hash of the name, size and mtime of every file it
took, in path order, which is what the watcher compares.

A **tenant** is an `Arc<Base>` — behind an `RwLock` so a reload can swap it —
plus an overlay, `BTreeMap<String, Option<FileData>>`: `Some` is a file the
tenant added or changed, `None` is a tombstone hiding a base file.
`Tenant::data` consults the overlay first, so a tombstone makes the base file
disappear; `Tenant::list` merges both views and flags overlay entries as
`modified`. Two tenants on the
same base share the base's memory; a tenant costs its overlay.

Each tenant has a **version**, a `u64` of milliseconds since the epoch at
creation. `write` and `delete` call `bump`, which sets it to
`max(now, previous + 1)` — strictly increasing — and sends
`{"type":"update"|"delete","path":…,"kind":…,"version":N}` on the tenant's
`tokio::sync::broadcast` channel (64 slots). Both SSE endpoints
(`/api/t/{id}/events`, `/__sl/events`) subscribe to that channel and open with
an `event: hello` carrying the current version.

The `kind` is what the preview does with the event — `css`, `style` or
`module`, in `UpdateKind`. `Tenant::write` takes it from the caller rather than
deciding: `UpdateKind::from_path` is the whole answer for a stylesheet, but
`style` takes a compile to prove, and that is `Engine::update_kind`'s job
(`src/transform/mod.rs`). See "CSS without a reload" below.

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
`Store::tenant` falls back to the same read on a miss, so a tenant another
daemon created under a shared `--data-dir` is restored the first time this one
is asked for it; a tenant already in memory is never re-read, which is the
limit `docs/multi-node.md` sets out. `--no-persist` keeps everything in memory.

Because that fallback makes the data directory as much a source of tenants as
the map, the two operations that decide whether a tenant exists consult both:
`Store::remove_tenant` drops the map entry and removes `<data-dir>/<id>`,
answering "no such tenant" only when neither holds it, and `create_tenant`
refuses an id whose directory is already there even when it cannot build a
tenant from it — its base may not be loaded on this node. `Store::tenants` is
the exception, and stays a list of what this daemon has loaded: it answers
`/api/tenants`, `/api/stats` and `/metrics`, none of which act on a tenant.

### Reloading a base

`Store::reload_base` re-reads a base from its own root, swaps the new
`Arc<Base>` into the store, and points every tenant on it at the same `Arc`
(`Tenant::set_base`, a `RwLock<Arc<Base>>`). Each of those tenants is bumped and
gets one `update` event, so open previews reload; overlays are untouched, so an
edited file still wins over the new base copy. The transform cache needs no
invalidation, being content-addressed: a changed file hashes to a new key.

`POST /api/bases/{name}/reload` calls it on the blocking pool.
`--watch-bases N` runs `Store::reload_changed_bases` every N seconds, also on
the blocking pool: one `stamp(root)` walk per base, and a reload only where the
stamp moved. The walk is the same function `Base::load` uses, so it skips
exactly what a load skips. A rewrite that changes neither the size nor the
mtime is invisible to it.

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
   tenant it serves that file — unless the path is private (`is_private_path`:
   any segment starting with `.`, `.well-known` excepted), which is also what
   keeps `/__sl/m/` and `/__sl/raw/` off a tenant's dotfiles. Otherwise it
   answers `assets/shell.html` with `%TENANT%`, `%VERSION%` (the tenant's
   version), `%ASSETS%` (a hash of `astro.js`, `shell.js`, `live.js`),
   `%TOKEN%`/`%TOKENQ%` (the preview token, empty without `--preview-secret`)
   and `%ENV%` filled in, `Cache-Control: no-store`. `%ENV%` is
   `import.meta.env`: `DEV`, `PROD`, `MODE`, `SSR`, `BASE_URL`, `SITE` (from
   `sandbox-lite.json`), `ASSETS_PREFIX`, and the `PUBLIC_*` lines of the
   tenant's `.env`.
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
   `astro`, `md`, `mdx` or `endpoint`, taken from the last extension only, so
   `rss.xml.ts` is the endpoint `/rss.xml`.
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
   The `client:component-path` of an island imported from a package is
   rewritten the same way, so the island's `component-url` is the CDN URL.
   The browser follows the graph; each request repeats step 4.
6. **Static paths.** If the route has params and the module exports
   `getStaticPaths`, the shell calls it with its own `paginate`, keeps the
   entry whose params equal the matched ones, and uses its `props`.
7. **Render.** `experimental_AstroContainer.create({ resolve, astroConfig })`
   — `resolve` maps ids the runtime asks for (island component paths,
   `astro:*`) to daemon URLs. `/__sl/renderers.json` lists framework renderers
   (`resolve::renderers`: `@astrojs/react`, `@astrojs/preact`, `@astrojs/vue`
   and `@astrojs/svelte` from `package.json`, or `sandbox-lite.json`
   `renderers`); each server renderer is imported and registered, each client
   entrypoint recorded. Astro's own
   `astro:jsx` renderer (`/__sl/shim/astro-jsx-runtime.js`, the JSX runtime
   plus `@astrojs/mdx/server.js`) is registered last; it renders MDX content.
   `container.renderToResponse(mod.default, { request, params, props })`
   produces the HTML.
   An endpoint route takes the same call with the module itself and
   `routeType: "endpoint"`, which runs its `GET` (or `ALL`) handler with an
   `APIContext` — `request`, `params`, `props`, `url`, `site`, `redirect`,
   `cookies`, `locals`. No renderers are registered for it: nothing renders.
8. **Document.** A `3xx` with `Location` becomes `location.replace`. A response
   that is not `text/html` (an endpoint's XML, JSON or text) is shown escaped in
   a `<pre>` under its status and content-type, JSON pretty-printed. A `4xx` or
   `5xx` with an empty body becomes an error overlay naming the status rather
   than a blank page, so a failed render never looks like a site with nothing
   on it (#48).
   Otherwise every CSS string that modules registered in
   `globalThis.__sl_css` becomes a `<style data-sl=key>` (with
   `type="text/tailwindcss"` when it contains `@import "tailwindcss"` or
   `@tailwind`), `live.js` is appended, the block is injected before
   `</head>`, and `document.write` replaces the shell. If Tailwind was seen,
   its browser build is loaded from jsDelivr. The `data-sl` key is what lets a
   later CSS write find the block again — see "CSS without a reload".
9. **Errors.** Any exception fetches `/__sl/check` and renders an error page
   with the daemon's diagnostics; a missing route lists the routes. Error
   pages also load `live.js`, so they reload when the file is fixed; their
   `<body data-sl-overlay>` is how `live.js` knows not to swap CSS into them.

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
| `.astro` | `astro_codegen` (`src/transform/astro.rs`): oxc parses, `<style lang="scss\|sass">` blocks are pre-compiled with grass, the compiler emits a module importing helpers from `/__sl/astro.js`; the CSS of each `<style>` and each hoisted `<script>` are kept aside; relative `client:component-path` values are made root-absolute, and the byte range of every package one is recorded for `serve` |
| `.ts` `.tsx` `.jsx` `.mts` | oxc transform (`src/transform/js.rs`): TypeScript stripped, JSX compiled |
| `.js` `.mjs` | served as-is when it parses as ESM, otherwise transformed |
| `.css` | a module that registers the text in `globalThis.__sl_css`, with relative `url()` and `@import` targets rewritten to `/__sl/raw/…` (`src/transform/css.rs`) |
| `.scss` `.sass` | grass over a snapshot of the tenant's file tree, on its own thread with a deadline (`src/transform/scss.rs`), then as `.css` |
| `.md` | frontmatter + pulldown-cmark HTML, wrapped as a page component that renders through `layout:` when set (`src/transform/markdown.rs`) |
| `.mdx` | satteri-mdxjs (`src/transform/mdx.rs`): frontmatter split off, headings given ids and collected, JSX compiled against `astro/jsx-runtime`, then wrapped as `@astrojs/mdx` does — `frontmatter`, `file`, `url`, `getHeadings`, a `layout:` wrapper, and a default `Content` export tagged for the `astro:jsx` renderer |
| `.json` | `export default JSON.parse(…)` |
| `.vue` `.svelte` | a loader module (`transform::sfc_loader`) that hands the source — its `<script lang="ts">` blocks already stripped to JavaScript by `transform::sfc` — to `/__sl/shim/vue-loader.js` or `svelte-loader.js`, which compiles it in the browser |
| images | `export default { src: "/__sl/raw/…", width, height, format, fsPath }` |
| `Style(i)` / `Script(i)` | builds the `.astro` module (cached) and returns its i-th CSS block or script as its own module |
| `Raw` / `Url` | the text as a string export / the `/__sl/raw/` URL as a string export |
| anything else | the `/__sl/raw/` URL as a string export |

### Vue and Svelte islands

There is no Rust compiler for `.vue` or `.svelte`, so `Kind::Module` returns a
three-line loader that carries the source as a string literal and calls
`/__sl/shim/vue-loader.js` or `/__sl/shim/svelte-loader.js` with it and with
`import.meta.url`. The loader runs `@vue/compiler-sfc` or `svelte/compiler` in
the browser and imports the result as a blob module.

The script the loader receives is always JavaScript. `transform::sfc` finds
every `<script>` element in the file, and for each one that says `lang="ts"`
runs its text through the same oxc transform as a `.ts` file — a `.vue` file's
`<script>` and `<script setup>`, a `.svelte` file's instance and
`context="module"` blocks, all of them. Only the `lang` attribute is dropped;
`setup`, `context` and the rest survive byte for byte, and the compiled script
is padded back to the line count of the block it replaces so the SFC compiler's
own diagnostics still name the right line of the template below it. A type
error is reported like any other file's, at its line in the `.vue` or
`.svelte` file, and `sandbox-lite check` sees it too.

Stripping ahead of `@vue/compiler-sfc` costs the macros that take a type and no
arguments — `defineProps<Props>()`, `defineEmits`, `defineModel`, `defineSlots`
and `<script setup generic="…">` — because the compiler reads those types out of
the source to generate the runtime declaration, and by then they are gone.
Rather than emit a component with no props, the daemon refuses the file with a
diagnostic that names the macro and asks for the runtime form. Doing better
means stripping *after* `compileScript`, which needs a TypeScript transform on
the browser side of the pipeline.

A blob module has no import map, so the loader rewrites the compiler's output
before creating the blob: `vue`/`svelte` specifiers become CDN URLs, relative
ones are resolved against the component's own `/__sl/m/…` URL. The CDN URLs are
substituted into the loader shim by `preview::shim` (`%VUE%`, `%VUE_COMPILER%`,
`%SVELTE%`) rather than baked into the cached module, because the transform
cache is content-addressed and does not see `package.json`. The compiled CSS
is registered in `globalThis.__sl_css` like any other module's, though the two
loaders key it differently: `vue-loader.js` registers one entry per `<style>`
block as `<path>?<index>`, the way an `.astro` style module is keyed, while
`svelte-loader.js` registers the component's single combined output under the
bare path.

The island imports that same module URL to hydrate, so the loader's default
export is the *client* build. Vue's one component object serves both — Vue's
server renderer falls back to the vdom for a component with no `ssrRender` —
while Svelte needs two builds, and the server one is attached to the client one
as `__sl_svelte_server` for `renderer-svelte.js` to find.

Both client entrypoints are shims of ours rather than the integration's:
`@astrojs/vue/client.js` imports a Vite virtual module, and
`@astrojs/svelte/client.js` ships uncompiled runes, so neither runs off a CDN.

A `Built` holds the compiled `body`, plus what `serve` needs later: `specs`
(byte ranges of every import specifier, from oxc's module record),
`component_paths` (byte ranges of every `client:component-path` /
`server:component-path` the compiler could not make root-absolute — a package
or a `tsconfig` alias — so an island resolves like any other specifier),
`globs` (byte ranges and options of every `import.meta.glob(...)` call,
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
invalidated explicitly. That is also why `build_bytes` can build a source the
tenant does not hold yet: classifying a write compiles the incoming bytes, and
the entry it leaves is the one the next module request would have paid for.

The cache (`Engine::cache`) is a `HashMap<u128, Arc<Built>>` behind a mutex
with a `VecDeque` of insertion order. The budget (`--cache-mb`, default 64
MiB) counts `body` bytes; when exceeded, the oldest-inserted entries are
dropped (FIFO, not LRU). `/api/stats` and `/metrics` report entries, bytes,
hits and misses.

### How many compiles run at once

A cache miss reserves `parser_stack_bytes()` of stack on a thread of its own,
and the request that asked for it sits on tokio's blocking pool, which would
let 512 of those exist together. So a miss takes one of `--max-compiles`
permits first (default: one per core, never fewer than one). A build that
finds none free waits five seconds for one and is then refused with 503 rather
than queueing without end. `/api/stats` reports the permits held, the builds
waiting, the limit and the refusals under `compiles`; `/metrics` has the same
four.

Two builds take no permit. A cache hit is not a compile, so a warm daemon
serving cached modules never queues. And a `?type=style` or `?type=script`
build asks for the module from inside its own compile: that inner build runs
under the permit — and on the stack — its parent is already holding, which a
thread-local marks for each. A second permit there would deadlock as soon as
the gate was full.

### Why resolution and `import.meta.glob` happen at serve time

The compiled body contains the specifiers as written — `./Header`,
`@/data/products`, `react` — and `import.meta.glob("./posts/*.md")` calls as
written. What they resolve to is a property of the tenant, not of the file:

- `./Header` is `Header.astro` in one tenant and `Header.tsx` in another
  (`Resolver::probe` tries extensions, then `index` files);
- the set of files matching `./posts/*.md` is the tenant's file list;
- `react-countup` is `https://esm.sh/react-countup@<range>` at the range that
  tenant's `package.json` pins;
- every emitted URL must carry the tenant's current `?v=`.

None of that can live in a content-addressed entry, so `Engine::serve` does it
on every request: it resolves each `spec` and each `component_path` with the
`Resolver`, expands each glob against `tenant.list()` (`glob::expand` writes
hoisted `import * as __sl_glob<i>_<j> from "/__sl/m/<file>?v=N"` statements for
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

### Sass limits

grass compiles synchronously and offers neither cancellation nor a resource
budget: `@while true {}` never returns. So every compile runs on a thread of
its own with a deadline (`--sass-timeout-ms`, default 5000) and the request
gives up on it rather than joining it. The compiler sees a `Snapshot`: the
tenant's file list with `FileData` handles, which copies no file content but
makes the file set `'static`, so the thread may outlive the request that
started it — and it fixes the file set for the whole compile, so nothing the
tenant writes mid-compile can change what an `@use` resolves to.

Sizes are capped on the way in and on the way out: 1 MiB per file, 4 MiB read
per compilation, 4 MiB of CSS produced. A file over a cap is hidden from the
compiler rather than failed through `Fs::read`, whose `io::Error` grass turns
into a panic; the reason is kept and becomes the error the request gets.

Compiles are keyed by source, directory and syntax. Requests that arrive while
one is running wait on it instead of starting a second, and once a compile has
passed its deadline every later request for the same source is refused until
the abandoned thread ends: a runaway stylesheet costs one thread, not one per
request. Eight threads may compile at once, which is the bound on distinct
runaway sources too. `/api/stats` counts them under `sass`.

None of this stops an abandoned compile — it keeps its core and keeps
allocating until it returns on its own. Only a child process with rlimits
would, and that is the design discussion in issue #26.

## Versions, browser cache and SSE

The version does three jobs:

- **Cache busting.** Every module URL the daemon emits carries
  `?v=<version at serve time>`, and `preview::module` marks such responses
  `immutable`. After an edit the version is higher, every URL is new, and the
  browser fetches fresh; the old URLs stay in its cache harmlessly. The daemon
  does not read the value of `v` — it only checks that the parameter is
  present — and always serves the file's current content.
- **Reload.** `live.js` reloads the page on any `update`/`delete` event it
  cannot answer by swapping CSS, and on the `hello` event when the daemon's
  version is higher than the one baked into the shell — which covers an edit
  that landed between the shell and the `EventSource` connecting, and a daemon
  restart, since restored tenants get a fresh version. A swap sets
  `window.__sl.version` to the version it applied, so a later reconnect does
  not read the page as stale.
- **Editor refresh.** The editor subscribes to `/api/t/{id}/events` to reload
  its file list and the open file.

The transform cache never sees the version; it is content-addressed.

## CSS without a reload

An `update` event carries a `kind`, and `live.js` swaps rather than reloads for
two of them.

- **`css`** — a `.css`, `.scss` or `.sass` write. The path is the key the CSS
  module registered, so `<style data-sl="src/styles/global.css">` is re-imported
  from `/__sl/m/<path>?v=<new version>` and its text replaced. A stylesheet the
  page reaches through another sheet's `@import` — `global.css` pulls in
  `tokens.css` that way — has no block of its own; the importing block's
  `/__sl/raw/<path>` URL gets a fresh `?v=` instead, which is what makes the
  browser fetch the file again.
- **`style`** — an `.astro` write whose compiled JS is byte-identical to the
  last build of that file, so only its `<style>` blocks moved. Each block is
  keyed `<file>?<index>` and re-imported from
  `?astro&type=style&index=<i>&lang.css`.
- Anything else reloads: a `module` kind, a `delete`, a `write_many`'s empty
  path, a `kind` the client does not know, a page showing the error overlay
  (`<body data-sl-overlay>`), a block the page does not have, or a failed swap.

`Engine::update_kind` decides. It compares the JS the incoming bytes compile to
against `last_js`, a bounded `(tenant, path) → fingerprint` map that every
module build of an `.astro` file writes — `Tenant::write` must stay cheap, and
only a build can prove the JS is unchanged. Three consequences worth knowing:
the fingerprint covers the module body *and* the hoisted `<script>` sources,
which the body only names by index, so editing a `<script>` reloads; the map is
written only for content the tenant holds, so a write that is then refused
leaves nothing behind to compare against; and a file nothing has built yet
reads as `module`, because a client that renders stale JS is worse than one
that reloads too often.

That compile takes a permit from the compile gate like any other, and a refused or failed
one reads as `module` — the fallback is always the reload.

Tailwind is the exception. A `type="text/tailwindcss"` block is compiled by
`@tailwindcss/browser` when that script loads, and it exposes no rebuild hook
to call afterwards, so any swap that would touch such a block reloads the page
instead.

## Content collections and Markdown (`src/transform/content.rs`)

`/__sl/content/<name>` returns `{entries, dates}`. An entry is `.md`/
`.markdown` with parsed YAML frontmatter as `data`, the body, the rendered
HTML and headings; `.mdx` with `data` and the body only; `.json` arrays as one
entry per item, other JSON as one entry; `.yaml`/`.yml` as one entry. The
`astro:content` shim fetches that JSON and implements `getCollection`,
`getEntry` and `render` on top of it; for an `.mdx` entry `render` imports the
compiled module at `/__sl/m/<filePath>` and returns its `Content`,
`getHeadings()` and `frontmatter`.

Every way the collection can fail to build answers **500** with the message and
a diagnostic, and the shim throws it so the shell's error overlay renders it: a
config past the size or nesting cap, a config that does not parse, a glob
pattern that does not compile, a `file()` loader whose source is absent or
malformed, and an entry file that is listed but cannot be read, whose JSON is
invalid or whose frontmatter is not a YAML mapping. An empty collection means a
collection with no entries and nothing else — a page rendered from a failed
build is indistinguishable from a page rendered from a tenant that has no posts
(issue #48).

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
for dates. That fallback is for a config the reader read and could not follow;
a config it could not read at all is the error above.

## `check` (`src/check.rs`)

`sandbox-lite check DIR…` loads each directory as a base, creates an
in-memory tenant on it, and calls `Engine::build(…, Kind::Module)` on every
source file under `src/` — `is_source`: `.astro`, `.ts`, `.tsx`, `.js`,
`.jsx`, `.mjs`, `.mts`, `.md`, `.mdx`, `.vue`, `.svelte`, which is what makes
the `.vue` type errors above reachable from the command line. It prints
diagnostics and a census — islands, glob calls,
Sass, MDX, endpoints, `@astrojs/*` integrations, bare imports — and exits 1 on
compile errors, 2 when a directory cannot be loaded. `/api/t/{id}/check` and
`/__sl/check` run the same loop (`api::check_tenant`) on a live tenant; the
error page uses the latter.

## Metrics (`src/metrics.rs`)

`Metrics` is a set of `AtomicU64`s with no dependencies: one counter per
(`Kind`, status class) for the module endpoint, and one histogram per `Kind`
for compile latency, over the fixed bucket set 1 ms, 5 ms, 20 ms, 50 ms,
100 ms, 500 ms, 1 s, 5 s. It lives in an `Arc` shared by `AppState` and
`Engine`, so `check` gets its own throwaway instance.

Only two places write to it. `preview::module` adds one to the counter for the
status it is about to return — a cache hit therefore costs one atomic add.
`Engine::build` times what a cache miss costs — the `on_parser_stack` hop onto
the big-stack thread and the `compile` it runs there, failures and a failed
spawn included; a hit is not a compile and is not timed. A `Style`/`Script`
compile calls `build` again for the module it needs, so on a cold cache the
module's time is counted once on its own and once inside the style's
observation.

`GET /metrics` (`api::metrics`) renders those counters plus the gauges
`/api/stats` already had — tenants, overlay bytes, cache entries and bytes,
SSE subscribers, RSS, uptime, one series per base, `Sass::stats`'s running,
runaway, timed-out and refused compilations, and the compile gate's permits
held, builds queued, limit and refusals — as Prometheus text format 0.0.4,
written by hand. Buckets are cumulative and `_count` is read off the `+Inf`
bucket rather than counted separately, so the two can never disagree. The same
numbers are in `/api/stats` under `modules`, `sass`, `compiles` and
`sse_subscribers`.

`require_api_token` exempts only `/` and `/health`, so `/metrics` needs the
bearer token whenever `--api-token` is set; `http::tests` asserts both halves.

## Chat (`src/http/ai.rs`)

`POST /api/t/{id}/chat` runs a tool loop against `{api_base}/v1/messages` —
`https://api.anthropic.com` unless `SANDBOX_LITE_ANTHROPIC_BASE` says
otherwise — with five tools scoped to the tenant: `list_files`, `read_file`,
`write_file`, `delete_file` and `check_site`. With `--chrome` set there is a
sixth, `screenshot` (`tools`), which renders one page of the tenant's own
preview in headless Chrome on the daemon's host and hands the PNG back as an
image block; `SECURITY.md` says what that costs. The loop runs at most 16
rounds (`MAX_ITERATIONS`) and answers
`{text, changes, iterations, version, chat}`. Writes go through the same
`Tenant::write` as the editor, so previews follow.

Every turn is streamed from the API whichever way the caller asked for it
(`converse`). With `Accept: text/event-stream` the endpoint answers SSE —
`text`, `tool`, `tool_result` events as they happen, then one `done` carrying
that same JSON, or `error`; otherwise the JSON is buffered and returned in one
response.

The request body is `{messages, chat?}`. `chat` names a stored conversation
(`src/http/chats.rs`, one JSON file per chat under `<data-dir>/<id>/chats/`, or
in memory with `--no-persist`), whose turns come before `messages`.
`Conversation::for_model` replays the last `--chat-window` turns in full; when a
conversation crosses that, `compact` folds everything older into one summary
written by a second call to the same API and stored with the conversation, and
sent as its opening turn from then on. A summary the API will not write is not
fatal: the turns stay stored and are cut from the request anyway. A save past
`--chats-per-tenant` drops the tenant's least recently updated conversation
(`evictable`).

## Layout

```
src/main.rs            flags, base loading, restore, listener
src/store.rs           Base, Tenant (overlay, version, events, persistence), Store, UpdateKind, clean_path, valid_id
src/http/mod.rs        routers, host dispatch, the two token middlewares, MIME table
src/http/api.rs        editor page, /api handlers, SSE, check_tenant
src/http/archive.rs    tar.gz export and import of a tenant tree
src/http/preview.rs    tenant-host handlers: shell, modules, raw, routes, renderers, shims, content
src/http/ai.rs         chat tool loop, the screenshot tool, conversation compaction
src/http/anthropic.rs  SSE frame reassembly and the streamed reply's content blocks
src/http/chats.rs      stored conversations: the window, the cap, load/save/list
src/metrics.rs         atomic counters, the compile histogram, the Prometheus renderer
src/resolve.rs         Resolver, CDN URLs, renderers, sandbox-lite.json, tsconfig paths
src/routes.rs          src/pages → route table
src/transform/mod.rs   Engine, cache, Built, serve-time splice
src/transform/astro.rs astro_codegen wrapper
src/transform/js.rs    oxc transform and import scanning
src/transform/css.rs   CSS-as-module, relative URL rewriting
src/transform/scss.rs  grass over a tenant snapshot, size and time limits, fingerprint
src/transform/glob.rs  import.meta.glob detection and expansion
src/transform/sfc.rs   TypeScript out of the <script> blocks of a .vue or .svelte file
src/transform/markdown.rs, content.rs   Markdown pages and collections
src/silent_failures.rs the check that reads src/ for failures answered as empty content (#48)
src/transform/mdx.rs   MDX pages and entries through satteri-mdxjs
src/check.rs           the check subcommand
assets/shell.html, shell.js, live.js    the browser side of a render
assets/editor.html     the editor
assets/astro.js        Astro runtime + container, built by scripts/build-runtime.sh
assets/astro-jsx.js    Astro's JSX runtime + astro:jsx renderer, built by the same script
assets/shims/          browser stand-ins for astro:* modules, the framework renderers and the Vue/Svelte SFC loaders
```
