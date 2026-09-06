import { expect, test } from './fixtures';
import { type Daemon, rawGet, startDaemon } from './daemon';

// --preview-secret and no --cookie-samesite override: the editor is on 127.0.0.1 and the preview
// on <id>.localhost, so the frame really is cross-site and a Lax cookie would never reach it.
test.describe('preview token', () => {
  let daemon: Daemon;
  let token: string;

  test.beforeAll(async () => {
    daemon = await startDaemon(['--preview-secret', 'e2e-preview-secret']);
    await daemon.tenant('framed', 'starter');
    const tenants = (await fetch(`${daemon.api}/api/tenants`).then((r) => r.json())) as { id: string; preview_token: string }[];
    token = tenants.find((t) => t.id === 'framed')!.preview_token;
    expect(token).toMatch(/^[0-9a-f]{32}$/);
  });

  test.afterAll(async () => {
    await daemon?.stop();
  });

  test('the editor frames a preview that renders', async ({ page }) => {
    // Nothing can put a token on `<link rel="icon" href="/favicon.svg">` in the layout, nor on the
    // imports inside a compiled module, so these are what prove the cookie reached the frame.
    const origin = `http://framed.localhost:${daemon.port}/`;
    const tokenless: string[] = [];
    page.on('response', (r) => {
      if (r.url().startsWith(origin) && !r.url().includes('sl_token')) tokenless.push(`${r.status()} ${r.url()}`);
    });

    await page.goto(`${daemon.api}/`);
    const frame = page.frameLocator('#frame');
    await expect(frame.locator('h1')).toHaveText('Design that ships.');
    await expect(frame.locator('.card')).toHaveCount(3);
    await expect(frame.locator('footer')).toContainText('rendered live by Astro');

    expect(tokenless.length, 'the frame fetched subresources with no token on the URL').toBeGreaterThan(0);
    expect(tokenless.filter((r) => Number(r.split(' ')[0]) >= 400), 'and every one of them was served').toEqual([]);

    // Served where it is rather than redirected, so the frame still holds the token it was opened with.
    const framed = page.frames().find((f) => f.url().startsWith(origin));
    expect(framed?.url()).toContain(`sl_token=${token}`);
  });

  // Sec-Fetch-* is chosen by the client, so claiming same-origin must buy nothing.
  test('a request with neither token nor cookie is refused, whatever it claims to be', async () => {
    const get = (path: string, headers?: Record<string, string>) => rawGet(daemon.port, 'framed.localhost', path, headers);
    const sameOrigin = { 'sec-fetch-site': 'same-origin', 'sec-fetch-dest': 'empty' };

    expect(await get('/')).toBe(403);
    expect(await get('/favicon.svg', sameOrigin)).toBe(403);
    expect(await get('/__sl/raw/.env', sameOrigin)).toBe(403);
    expect(await get('/__sl/m/src/pages/index.astro?v=1', sameOrigin)).toBe(403);
    expect(await get(`/?sl_token=${'0'.repeat(32)}`)).toBe(403);
    // A client with no cookie jar still gets in on the token alone.
    expect(await get(`/__sl/routes.json?sl_token=${token}`)).toBe(200);
  });

  test('a top-level navigation with the token still lands on the clean URL', async ({ browser }) => {
    const context = await browser.newContext();
    const page = await context.newPage();
    await page.goto(`http://framed.localhost:${daemon.port}/about?sl_token=${token}`);
    await expect(page).toHaveURL(`http://framed.localhost:${daemon.port}/about`);
    await expect(page.locator('h1')).toHaveText('About the studio');
    await context.close();
  });
});
