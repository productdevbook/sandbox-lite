# Security

This document describes what sandbox-lite enforces, what it leaves to the
operator, and how to report a vulnerability. Every statement below is about the
code in `src/` and `assets/` as it is; where the README is looser, this
document wins.

## Reporting a vulnerability

Report privately through GitHub's vulnerability reporting for this repository:
<https://github.com/productdevbook/sandbox-lite/security/advisories/new>
(Security tab → Advisories → Report a vulnerability). Do not open a public
issue for anything that lets one tenant read or write another tenant, reach the
editor or API from a preview host, or read files outside a tenant's tree.

Include the commit (`git rev-parse HEAD` — there are no tagged releases; `main`
is the only supported line), the flags the daemon was started with, and a
reproduction as requests: `Host` header, path, body, and the tenant files
involved.

## What runs where

One process, one listener (`--listen`, default `127.0.0.1:4321`). The `Host`
header of each request decides which of two routers answers
(`dispatch` and `tenant_from_host` in `src/http/mod.rs`):

| Host | Router | Contents |
|---|---|---|
| `<id>.<domain>` — exactly one label before `--domain`, and the label is a valid tenant id (1–63 bytes of `a-z`, `0-9`, `-`, no leading or trailing dash) | tenant | the shell page, `public/` files and everything under `/__sl/` for that tenant |
| anything else — the bare domain, the daemon's IP address, a name with two labels, a missing `Host` header | editor / API | `/` (the editor), `/health`, `/metrics`, `/api/*` |

So the editor and the API are not only "on the bare domain": they answer on
every hostname that does not parse as a tenant host. Put the daemon behind a
proxy that only forwards the hostnames you intend, and treat every request
that reaches the editor/API router as coming from your own backend.

Serving a preview evaluates no tenant code in the daemon. The daemon compiles
`.astro` (astro_codegen), TypeScript/JSX (oxc), Sass (grass), Markdown
(pulldown-cmark) and parses JSON/YAML; the resulting JavaScript runs in the
visitor's browser on the tenant origin, using Astro's own runtime bundled as
`/__sl/astro.js`. One flag changes that.

Whoever can call `/api/*` owns every tenant: create, delete, read and write any
file, drive the chat endpoint. They also decide what the daemon reads off the
host as a base project, bounded only by `--bases` (below). There are no users,
roles or per-tenant credentials on the API side.

### What `--chrome` adds

`--chrome PATH` gives the chat model a sixth tool, `screenshot` (`tools`,
`src/http/ai.rs`). Calling it spawns the operator's Chrome binary **on the
daemon's host** and points it at the tenant's own preview URL (`shoot`, and
`preview_url_path` in `src/http/api.rs`). That browser then executes the
tenant's compiled modules, its inline `<script>` blocks and everything they
import from `--cdn`, as the daemon's user. Three consequences:

- Chrome is started with `--no-sandbox`, so its renderer sandbox is off: a
  renderer bug reaches the daemon's uid directly rather than a sandbox.
- Tenant JavaScript gets the daemon's host and network position for the length
  of the run — loopback services, a cloud instance-metadata endpoint, anything
  the daemon's egress reaches. The 20 s deadline (`SHOT_TIMEOUT`) bounds how
  long that lasts, not what it can reach.
- The tenant's `sl_token` is passed to Chrome as a command-line argument, so it
  is in the host's process table for the life of the browser.

The URL itself stays on the tenant: `shot_path` refuses `://`, a leading `//`,
whitespace and control characters, and the origin is built from the tenant id
rather than from the model's input. What runs *on* that page is the exposure.

The path to it is: whoever can call `POST /api/t/{id}/chat` → the model → a
page of that tenant. So enable `--chrome` only where the tenant trees are
trusted, and run the browser somewhere it cannot reach anything else: a
container, or a network namespace of its own.

## What the daemon enforces

- **Paths.** Every file path taken from a request — `/api/t/{id}/file/{path}`,
  `/__sl/m/{path}`, `/__sl/raw/{path}`, the page fallback that serves
  `public/`, and the paths the chat tools receive — goes through `clean_path`
  (`src/store.rs`): the leading `/` is dropped, empty and `.` segments are
  removed, and the path is rejected if it is empty, contains `\` or a NUL
  byte, or has any `..` segment. Paths are relative to the tenant tree and
  cannot leave it.
- **Tenant ids** are validated by `valid_id` (same rule as the host label) and
  are used as directory names under `--data-dir`.
- **Deleting a tenant** (`DELETE /api/tenants/{id}`) resolves it the way every
  other tenant route does — this daemon's memory first, then `<data-dir>/<id>`
  — and removes both. A tenant the daemon can serve but has not loaded is
  deleted rather than answered `404` and then restored by the next request
  (issue #74). What goes is the whole directory: the overlay files, the
  tombstone list and the tenant's conversations. `204` means the bytes are gone
  from that data directory; a second daemon that already holds the tenant in
  memory keeps serving its own copy (`docs/multi-node.md`).
- **Base projects** are read at startup, and again on
  `POST /api/bases/{name}/reload` or a `--watch-bases` tick. `node_modules`,
  `.git`, `dist`, `.astro`, `.vercel`, `.netlify` and `.output` are skipped,
  and so is anything that is not a regular file or directory (symbolic links
  are not followed) — by the reload and the poll exactly as by the first read,
  since all three walk the same function. The top of the tree is held to the
  same rule: the `--bases` scan takes an entry only when the entry itself is a
  directory (`Store::bases_in_dir` reads `DirEntry::file_type`, which does not
  resolve a link), so a symbolic link sitting in `--bases` is skipped whatever
  it points at and named on stderr. Each root it does take, and each
  `--base NAME=PATH`, then goes through `Store::base_root` — the containment
  `POST /api/bases` applies — so a base outside `--bases` is refused however it
  arrives: a `--base` outside it stops startup, an entry of the scan that
  resolves out of it is skipped and named on stderr (issue #73).
- **Adding a base at runtime** (`POST /api/bases` with `{"name", "path"}`) is
  an operator-only surface behind `--api-token`, and nothing else gates it.
  The name must pass `valid_id`. The path is canonicalized, must be a
  directory, and must resolve inside `--bases` when that flag is set — which
  refuses a symbolic link out of it, since the link is resolved before the
  check. **Without `--bases` there is no containment at all**: any directory
  the daemon's user can read becomes a base, and every file in it is then
  served to whoever can open a preview on a tenant created from it (see
  "Tenant files are public to anyone who can open the preview"). Set `--bases`
  on any daemon whose API is reachable by anything but your own backend.
- **Request bodies** on the editor/API router are capped at 64 MiB
  (`DefaultBodyLimit`). No tenant-host handler reads a request body.
- **Compiled source** is bounded three ways before it reaches oxc,
  `astro_codegen`, `satteri-mdxjs` or `grass`, all of which are
  recursive-descent and so can overflow a thread stack — which aborts the whole
  daemon, since the release profile sets `panic = "abort"`. The three apply to
  the extensions those parsers read (`parses_source`, `src/transform/mod.rs`):
  `.astro`, `.ts`, `.tsx`, `.jsx`, `.mts`, `.js`, `.mjs`, `.mdx`, `.scss`,
  `.sass`, `.vue` and `.svelte` — the last two because `sfc::check_vue` and
  `sfc::strip_types` run oxc over their `<script lang="ts">` blocks.
  - **Size** is capped by `--max-source-kb` (default 64 KiB).
  - **Nesting** is capped at 2000 (`MAX_NESTING_DEPTH`) — the deepest run of
    unclosed `(`, `[`, `{` or of markdown blockquote markers, counted on the
    bytes before any parser sees them.
  - **Stack**: every compile runs on a thread whose stack is sized from the
    size cap — `max_source_bytes * 4000`, never below 16 MiB
    (`Engine::parser_stack_bytes`), so 250 MiB at the default — which
    covers the recursion the nesting scan does not model — chained unary `-`
    for oxc, `<<` for `astro_codegen`. Raising `--max-source-kb` raises the
    stack with it. The reservation is virtual address space; only the pages a
    compile touches become resident.

  Past either cap the build answers with a compile diagnostic. `.md`, `.css`,
  `.json`, images and `?raw`/`?url` requests are not capped: none of them
  recurse over the source.

  Two parsed sources do not reach grass or oxc through `Engine::build`, and get
  the bounds where they are loaded instead:
  - `src/content.config.ts`, read by the content route — past the size or the
    nesting cap the collection answers 500 with the diagnostic. It used to fall
    back to the directory layout, which served a config nobody had read as if
    the tenant had written no config at all (issue #48).
  - Sass partials, which grass reads itself and compiles on its own thread
    (see below) rather than on the one `Engine::build` reserved. Each is
    refused past the nesting cap, and that thread gets a 64 MiB stack.
- **`POST /__sl/strip-ts`** is the one route that compiles bytes the caller
  sends rather than a file the tenant holds: `@vue/compiler-sfc` runs in the
  browser and posts the script it generated back for its TypeScript to be
  removed (issue #52). It is behind the preview token like everything under
  `/__sl/`, refuses a body over 4 MiB before reading it, and inside
  `Engine::strip_ts` is held to the nesting cap, a size cap of
  `--max-source-kb` times eight (a compiled script outgrows the file it came
  from), a `--max-compiles` permit and the `--compile-timeout-ms` deadline —
  the same bounds a module build gets. What it does not have is a bound on how
  often it may be called; see "Limits that do not exist".
- **Concurrent compiles** are capped by `--max-compiles` (default: one per
  core). Each compile holds a stack reservation of its own, so without the cap
  the only ceiling on how many exist together is tokio's blocking pool of 512
  threads. A build that waits more than five seconds for a permit is refused
  with 503. A cache hit takes no permit, and the module build a `?type=style`
  request makes from inside its own compile runs under its parent's.
- **Host matching** is exact: `a.b.<domain>` and `evil-<domain>` are not tenant
  hosts.
- **Sass compilation** is bounded (`src/transform/scss.rs`). grass runs
  synchronously and cannot be cancelled — `@while true {}` never returns — so
  each compile runs on a thread of its own with a deadline
  (`--sass-timeout-ms`, default 5000) that the request gives up on rather than
  joins. A source over 1 MiB is refused before the compile starts, one
  compilation may read at most 4 MiB through `@use`/`@import`, and CSS over
  4 MiB is an error instead of a cache entry. The thread that overran cannot be
  stopped and is left to finish; while it runs, every further request for the
  same source is refused, so a runaway stylesheet costs one thread rather than
  one per request, and at most 8 compiles run at once whatever their source.
  `/api/stats` reports `sass.running`, `sass.runaway`, `sass.timeouts` and
  `sass.refused`. What this does not do is stop an abandoned compile: it keeps
  a core and keeps allocating until it ends by itself, so
  `@while true { .a { color: red } }` can still exhaust memory, and eight of
  them stop Sass compiling for every tenant until the daemon restarts. Bounding
  that needs the compile in a child process with rlimits.
- **The preview cookie** (`sl_t`) is set with `Path=/; HttpOnly` and no
  `Domain` attribute, so it is a host-only session cookie for that one tenant
  host, and its value is the tenant's token. Every response to a request that
  carried a valid token sets it. `--cookie-samesite` picks `SameSite=Lax`
  (`Secure` added only when the request carried `x-forwarded-proto: https`) or
  `SameSite=None; Secure`; with `--preview-secret` the default is `none`,
  because a browser stores no other form for a cross-site frame and the editor
  frames its previews from another host. Browsers accept `Secure` over HTTPS
  and on loopback origins (`localhost`, `127.0.0.1`, `*.localhost`), so the
  default development setup works; a plain-`http` deployment on a public name
  cannot hold the cookie, and only requests carrying the token in the URL will
  be served there.
- **Connections** that have not delivered a complete request head within 30 s,
  whether newly opened or idle between keep-alive requests, are closed.
- **Archive imports.** `POST /api/tenants/{id}/import` is on the editor/API
  router, so `--api-token` is the only thing gating it, and the tenant must
  already exist — an import cannot create one. Every tar entry is checked
  before anything is written: only regular files are taken, directory and
  pax-header entries are skipped, and a symbolic link, hard link, device or
  fifo entry is refused with `400` naming it. The entry name must be
  tenant-relative: a leading `/`, a `..` segment, a backslash or a NUL byte is
  refused with `400` naming the entry — an absolute path is refused, not
  stripped the way `clean_path` strips one from a request path — while `./` and
  `//` segments are normalised away. So an import writes only under
  `<data-dir>/<id>/files`. Every entry costs its declared size against the
  quota whether or not it is taken — `tar` reads those bytes to reach the next
  header either way — and the import is refused with `413` as soon as the
  running total passes the quota, before that entry's body is read. The
  decompressor is capped on top of that, at the quota plus 16 MiB of tar
  framing and never more than a thousand times the compressed body, which is
  what bounds the bytes `tar` reads without ever surfacing them as an entry: a
  GNU long name, a pax payload, padding. So an archive that decompresses past
  the quota is rejected without being decompressed. What survives is then
  applied under the tenant's write lock as one batch, quota-checked against the
  resulting overlay. The batch is all or nothing: the disk work is staged with
  an undo — a file it replaces or removes is moved aside, not deleted — and the
  overlay is swapped only once every file has landed, so a refusal or an I/O
  failure leaves the tenant exactly as it was, on disk as well as in memory,
  and the response says so. If the undo itself fails the response says that
  instead, naming the paths that are neither way, rather than claiming a clean
  refusal.
- **Hidden files** are exported and imported like any other file: an export
  carries `.env`, `sandbox-lite.json` and every dotfile of the tenant, and an
  import may write them. What the preview refuses to serve is unchanged
  (`is_private_path`), and both routes are on the editor/API router, whose
  audience is already every file of every tenant.
- **Per-tenant write volume** is capped by `--tenant-quota-mb` (default 64)
  and by `--tenant-max-files` (default 10000): a write that would take the
  tenant's edited files past either is refused with `413` before anything
  reaches disk or memory, and the chat `write_file` tool and the import get the
  same error. Rewriting a file is charged only for the difference. The count is
  a separate limit because bytes do not bound it — an empty file weighs nothing
  and still costs an inode and a directory entry.
- **Conversations** are outside that quota, so they have their own limits:
  `--chat-message-kb` (default 64) is the most one request's `messages` may
  weigh and `--chat-quota-kb` (default 2048) the most one conversation may
  hold, both refused with `413` before the model is called and before anything
  is stored; `--chats-per-tenant` (default 50) drops a tenant's least recently
  updated conversation when a save would take it past the cap or past
  `--chats-per-tenant` × `--chat-quota-kb` of chats directory; and
  `--chat-window` (default 24) is how many stored turns reach the model — older
  turns are folded into a stored summary, and are cut from the request even
  when that summary could not be written. `/api/stats` reports what the chats
  directory holds across every tenant it has loaded, and the ceiling it is held
  to (`Store::tenants` is this daemon's memory, not a census of `--data-dir`).
- **The screenshot tool** runs `--chrome-jobs` browsers at once (default 1).
  A call that waits 10 s without a slot is answered "busy" rather than queued,
  and one whose browser overruns its 20 s deadline is killed and reaped before
  its profile directory and PNG are removed. `sandbox_lite_screenshots_*`
  counts what is running, what was refused and what was killed.

The daemon sets no CORS, CSP, `X-Frame-Options` or `Referrer-Policy` headers.
Cross-origin reads between tenant hosts are blocked by the browser's
same-origin policy because no CORS headers are sent; framing and navigation
between hosts are not restricted.

## The two authentication flags

Both are off unless set. `SANDBOX_LITE_API_TOKEN` and
`SANDBOX_LITE_PREVIEW_SECRET` are read as defaults (empty values are ignored).
The Docker image sets neither, so a container started without environment
variables is fully open on `0.0.0.0:4321`.

### `--api-token TOKEN` (`require_api_token`, `src/http/mod.rs`)

Wraps the editor/API router. `/` and `/health` are exempt; everything else on
that router answers `401` unless the request carries
`Authorization: Bearer TOKEN` or `?token=TOKEN`.

- It is one shared secret. There is no way to hand a caller access to a single
  tenant through the API.
- The query form exists because `EventSource` cannot set headers (the editor
  uses it for `/api/t/{id}/events`); a token in a URL ends up in proxy and
  access logs.
- The editor page keeps the token in `localStorage` (`sl_api_token`) on the
  editor origin.
- It gates spending: every `POST /api/t/{id}/chat` can make up to 16 calls to
  the Anthropic API with the daemon's `ANTHROPIC_API_KEY`, and one more when
  the conversation has to be summarized.
- It is the only thing gating `POST /api/tenants/{id}/import`, which writes a
  whole file tree into a tenant in one request, and
  `GET /api/t/{id}/export`, which returns one.
- It is the only thing gating `GET /api/t/{id}/chats` and
  `GET /api/t/{id}/chats/{chat}`, which return a tenant's stored conversations
  in full — including whatever the model quoted out of the tenant's files.
- It is the only thing gating `POST /api/bases`, which reads a directory of
  the host into memory, and `POST /api/bases/{name}/reload`, which re-reads
  one and makes every tenant on it reload.
- It does nothing for tenant hosts.

### `--preview-secret S` (`require_preview_token`, `src/http/mod.rs`)

Wraps the whole tenant router: the shell, `public/` files and every `/__sl/`
endpoint, including `/__sl/raw/`, `/__sl/events` and `/__sl/check`.

- The per-tenant token is `preview_token(S, id)`: the first 32 hex characters
  of a BLAKE3 keyed hash of the tenant id, with the key derived from `S`. It
  is a pure function of the secret and the id: it never expires and cannot be
  revoked for one tenant. Rotating `S` invalidates every tenant's link at once.
- `GET http://<id>.<domain>/any/path?sl_token=<token>`: a wrong token answers
  `403`; the right one is served, and the response sets the `sl_t` cookie
  described above. A top-level navigation — `Sec-Fetch-Dest: document`, which a
  frame reports as `iframe` — answers `303` to the same path with the parameter
  removed, so a link a person opens becomes a clean URL. A frame is served
  where it is instead: redirecting it would take the token out of the URL
  before anything on the page had it. `Sec-Fetch-Dest` decides only that, never
  access; it is a value the client chooses.
- **A request carrying neither a valid token nor the cookie answers `403`.**
  Nothing else is accepted — no request header, no referrer, no origin. The two
  comparisons, like the API token's, take constant time (`constant_time_eq`).
- What the cookie is for: `<link rel="icon" href="/favicon.svg">` in a layout
  and the imports inside a compiled module carry no token and nothing can put
  one there, so the cookie is what serves them. This is why the `SameSite=None;
  Secure` default matters — it is the only cookie a browser sends from a
  cross-site frame.
- The shell also puts the token in `window.__sl.token` and carries it on the
  `/__sl/**` requests it issues itself, so a browser that will not store the
  cookie still renders. Tenant JavaScript can read it there, which the
  `HttpOnly` cookie did not allow — but tenant JavaScript already runs on that
  host with the cookie attached to every request it makes, so the token gives
  it nothing it did not have.
- **Where the token in a URL ends up.** Every preview-host response carries
  `Referrer-Policy: no-referrer` and the shell repeats it in a `<meta>`, so a
  tenant page never names its own URL to `esm.sh` or any other third party. The
  residue that cannot be removed: a token that reached the browser as a URL is
  in that browser's history, and in the access log of any proxy in front of the
  daemon. Since a token is a pure function of the secret and the tenant id, it
  never expires and cannot be revoked for one tenant — rotating `S` is the only
  way to invalidate one, and it invalidates every tenant's link at once.
- `/api/tenants` returns each tenant's `preview_token` and a ready-made
  `preview` link, so anyone with API access can open every preview.
- The `preview` link the API builds is
  `http://<id>.<domain>:<port>/?sl_token=…` — plain `http` and the daemon's
  own listen port, whatever proxy is in front. Behind TLS, build the link
  yourself from `preview_token`.
- It does nothing for the editor/API router.

## Tenant files are public to anyone who can open the preview

`/__sl/raw/<path>` returns any file of the tenant (base project plus overlay)
as-is, and `/__sl/m/<path>?raw` returns it as a module. That includes
`package.json`, `sandbox-lite.json`, `tsconfig.json` and every source file, and
the compiled output of every page and component, because that is what the
browser renders.

Dotfiles are the exception. `is_private_path` (`src/store.rs`) refuses any path
with a segment starting with `.`, `.well-known` excepted, and it gates all
three preview read paths: `/__sl/m/{path}`, `/__sl/raw/{path}` and the
`public/` fallback (`src/http/preview.rs`). A preview visitor cannot fetch
`.env`.

Put no secrets in base projects or tenant trees anyway. A dotfile is not a
secret store, and three things reach past that rule:

- The `PUBLIC_*` keys of `.env` are read into the shell's `import.meta.env`
  (`env_map`, `src/http/preview.rs`) and exported by the `astro:env` shim, so
  they are in the page every visitor loads.
- The editor/API router serves the file itself: `GET /api/t/{id}/file/.env`
  goes through `clean_path` alone, and the chat's `read_file` tool does the
  same. Both are behind `--api-token`, whose audience is already every file of
  every tenant.
- An export carries every dotfile of the tenant (see "Hidden files" above).

With `--preview-secret` the preview's audience is "whoever has the tenant's
link"; without it, the audience is the network.

## Put previews on their own registrable domain

Tenant JavaScript runs on `<id>.<domain>`. The daemon's own cookie is
host-only, but cookies set by the operator's other services are governed by
browser rules, not by the daemon:

- A cookie set with `Domain=example.com` is sent by the browser with every
  request a page on `acme.preview.example.com` makes to `*.example.com`, and
  is readable through `document.cookie` unless it is `HttpOnly`.
- `SameSite=Lax` and `SameSite=Strict` compare registrable domains.
  `acme.preview.example.com` and `app.example.com` are the same site, so
  `SameSite` does not stop a tenant page from making credentialed requests to
  `app.example.com`.

The consequence: never set a cookie whose `Domain=` covers the preview hosts,
and, because the second point holds even for host-only cookies on your
product's domain, run previews on a separate registrable domain
(`example-preview.net`, not `preview.example.com`) or on a name registered on
the Public Suffix List. Keep the editor and the API off that domain: the
editor stores the API token in `localStorage` and embeds tenant previews in an
`<iframe>`, which is safe only while the two are different origins.

## What leaves the machine

- **Anthropic API.** When `ANTHROPIC_API_KEY` is set, `POST /api/t/{id}/chat`
  sends to `{api_base}/v1/messages` (`src/http/ai.rs`), where `api_base` is
  `https://api.anthropic.com` unless `SANDBOX_LITE_ANTHROPIC_BASE` names
  another endpoint (`src/main.rs`). What is sent: the conversation the caller
  supplies, the stored turns of the conversation it continues, a system prompt
  containing the tenant id and base name, and every tool result — the file
  listing, the contents of any file the model reads (up to 200 KiB per file,
  any path in the tenant, dotfiles included), `check` diagnostics, and, under
  `--chrome`, a PNG of the rendered page up to 4 MiB (`MAX_SHOT`) base64-encoded
  into the tool result. `write_file` and `delete_file` take effect on the tenant
  immediately, without confirmation. A conversation that has passed
  `--chat-window` costs one further call to the same endpoint, which sends the
  turns being folded away and takes back the summary that replaces them.
  Without the key the endpoint answers `503` and nothing is sent.
- **CDN, in the visitor's browser.** Bare imports (`react`, `dayjs`) resolve to
  `--cdn` (default `https://esm.sh`) with the version range from
  `package.json`; React and Preact client entrypoints come from the same CDN.
  `sandbox-lite.json` `imports` can map a specifier to any URL, and tenant
  code can import any absolute URL directly. Tailwind's browser build is
  always loaded from `https://cdn.jsdelivr.net/npm/@tailwindcss/browser@4`
  (hard-coded in `assets/shell.js`); `--cdn` does not change that.

## Limits that do not exist

- No rate limiting on any route.
- **No deadline on a single non-Sass compile.** `--max-compiles` bounds how
  many run at once (above), and Sass has a deadline of its own, but an
  `.astro`, `.ts` or `.mdx` compile runs to completion however long it takes,
  holding its permit throughout. Nothing bounds the total CPU a caller can ask
  for over time either: compilation is CPU work per request, and `/__sl/check`
  and `/api/t/{id}/check` build every source file of a tenant on every call — a
  cache hit for an unchanged file, a compile for a changed one.
- No timeout once a request head has arrived: a body may trickle in, and a
  response may be read slowly, for as long as the client likes.
- The transform cache is bounded by `--cache-mb` and each tenant's overlay by
  `--tenant-quota-mb`, but the number of tenants is not: a caller with API
  access can create any number of them, each holding up to the quota. With
  persistence every write goes to disk under `--data-dir`, and files over
  256 KiB are kept only there and re-read on demand; with `--no-persist`,
  every written file is held in memory whatever its size.
- Tenant code is limited only by the visitor's browser — except under
  `--chrome`, where the screenshot tool runs it in a browser on the daemon's
  own host, unsandboxed (see "What `--chrome` adds").

## Deployment checklist

1. Previews on their own registrable domain; wildcard DNS for it; `--domain`
   set to that name.
2. Editor and `/api/*` reachable only from your backend, on a hostname the
   proxy does not expose; `--api-token` set as well; `--bases` set, so a base
   added over the API cannot come from anywhere else on the host.
3. `--preview-secret` set; links built from `preview_token` with your scheme
   and host; the TLS proxy sends `x-forwarded-proto: https` so the cookie is
   `Secure`.
4. No secrets in base projects or tenant files.
5. Your own limits on tenant count and request rate; `--tenant-quota-mb`
   bounds each tenant's write volume, not how many tenants there are.
6. `ANTHROPIC_API_KEY` only if the chat endpoint is wanted, knowing what it
   sends and that the API token is the only thing gating it;
   `SANDBOX_LITE_ANTHROPIC_BASE` set only to an endpoint you trust with all of
   that.
7. `--chrome` only where the tenant trees are trusted, and then with the
   browser confined — a container, or a network namespace — because it runs
   tenant JavaScript on the daemon's host, unsandboxed.
