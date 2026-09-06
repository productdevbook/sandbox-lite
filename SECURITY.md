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

Tenant files never run inside the daemon. The daemon compiles `.astro`
(astro_codegen), TypeScript/JSX (oxc), Sass (grass), Markdown (pulldown-cmark)
and parses JSON/YAML; the resulting JavaScript runs in the visitor's browser on
the tenant origin, using Astro's own runtime bundled as `/__sl/astro.js`.

Whoever can call `/api/*` owns every tenant: create, delete, read and write any
file, drive the chat endpoint. They also decide what the daemon reads off the
host as a base project, bounded only by `--bases` (below). There are no users,
roles or per-tenant credentials on the API side.

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
- **Base projects** are read at startup, and again on
  `POST /api/bases/{name}/reload` or a `--watch-bases` tick. `node_modules`,
  `.git`, `dist`, `.astro`, `.vercel`, `.netlify` and `.output` are skipped,
  and so is anything that is not a regular file or directory (symbolic links
  are not followed) — by the reload and the poll exactly as by the first read,
  since all three walk the same function.
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
  the extensions those parsers read: `.astro`, `.ts`, `.tsx`, `.jsx`, `.mts`,
  `.js`, `.mjs`, `.mdx`, `.scss`, `.sass`.
  - **Size** is capped by `--max-source-kb` (default 64 KiB).
  - **Nesting** is capped at 2000 (`MAX_NESTING_DEPTH`) — the deepest run of
    unclosed `(`, `[`, `{` or of markdown blockquote markers, counted on the
    bytes before any parser sees them.
  - **Stack**: every compile runs on a thread whose stack is sized from the
    size cap (`Engine::parser_stack_bytes`, 256 MiB at the default), which
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
    nesting cap it is left unparsed and the collection falls back to the
    directory layout.
  - Sass partials, which grass reads itself and compiles on its own thread
    (see below) rather than on the one `Engine::build` reserved. Each is
    refused past the nesting cap, and that thread gets a 64 MiB stack.
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
  `<data-dir>/<id>/files`. Entry sizes are summed from the tar headers as
  the archive is read and the whole import is refused with `413` once the total
  passes the quota, so an archive that decompresses past it is rejected without
  being decompressed. What survives is then applied under the tenant's write
  lock as one batch, quota-checked against the resulting overlay: a refusal
  writes nothing, and the response says so.
- **Hidden files** are exported and imported like any other file: an export
  carries `.env`, `sandbox-lite.json` and every dotfile of the tenant, and an
  import may write them. What the preview refuses to serve is unchanged
  (`is_private_path`), and both routes are on the editor/API router, whose
  audience is already every file of every tenant.
- **Per-tenant write volume** is capped by `--tenant-quota-mb` (default 64):
  a write that would take the tenant's edited files past it is refused with
  `413` before anything reaches disk or memory, and the chat `write_file` tool
  gets the same error. Rewriting a file is charged only for the difference.
- **Conversations** are outside that quota, so they have their own two limits:
  `--chats-per-tenant` (default 50) drops a tenant's least recently updated
  conversation when a save would take it past the cap, and `--chat-window`
  (default 24) is how many turns of one conversation reach the model — older
  turns are folded into a stored summary, and are cut from the request even
  when that summary could not be written. `/api/stats` reports what the chats
  directory holds across every tenant.
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
as-is, and `/__sl/m/<path>?raw` returns it as a module. That includes `.env`,
`package.json`, `sandbox-lite.json` and every source file. Only the shell's
`import.meta.env` is filtered to `PUBLIC_*` keys; the `.env` file itself is
not. The compiled output of every page and component is served too, because
that is what the browser renders.

Do not put secrets in base projects or tenant trees. With `--preview-secret`
the audience is "whoever has the tenant's link"; without it, the audience is
the network.

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
  sends to `https://api.anthropic.com/v1/messages`: the conversation the
  caller supplies, a system prompt containing the tenant id and base name, and
  every tool result — the file listing, the contents of any file the model
  reads (up to 200 KiB per file, any path in the tenant), and `check`
  diagnostics. `write_file` and `delete_file` take effect on the tenant
  immediately, without confirmation. A conversation that has passed
  `--chat-window` costs one further call, which sends the turns being folded
  away and takes back the summary that replaces them. Without the key the
  endpoint answers `503` and nothing is sent.
- **CDN, in the visitor's browser.** Bare imports (`react`, `dayjs`) resolve to
  `--cdn` (default `https://esm.sh`) with the version range from
  `package.json`; React and Preact client entrypoints come from the same CDN.
  `sandbox-lite.json` `imports` can map a specifier to any URL, and tenant
  code can import any absolute URL directly. Tailwind's browser build is
  always loaded from `https://cdn.jsdelivr.net/npm/@tailwindcss/browser@4`
  (hard-coded in `assets/shell.js`); `--cdn` does not change that.

## Limits that do not exist

- No rate limiting on any route.
- Compilation is CPU work per request; `/__sl/check` and `/api/t/{id}/check`
  build every source file of a tenant on every call — a cache hit for an
  unchanged file, a compile for a changed one. Only Sass has a deadline and a
  thread cap (above); nothing bounds the total CPU a caller can ask for.
- No timeout once a request head has arrived: a body may trickle in, and a
  response may be read slowly, for as long as the client likes.
- The transform cache is bounded by `--cache-mb` and each tenant's overlay by
  `--tenant-quota-mb`, but the number of tenants is not: a caller with API
  access can create any number of them, each holding up to the quota. With
  persistence every write goes to disk under `--data-dir`, and files over
  256 KiB are kept only there and re-read on demand; with `--no-persist`,
  every written file is held in memory whatever its size.
- Tenant code is limited only by the visitor's browser.

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
   sends and that the API token is the only thing gating it.
