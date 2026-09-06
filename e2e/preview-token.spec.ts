import { expect, test } from './fixtures';
import { type Daemon, startDaemon } from './daemon';

// --preview-secret with the default Lax cookie: the editor is on 127.0.0.1 and the preview on
// <id>.localhost, so the frame is cross-site and the cookie never reaches it.
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
    await page.goto(`${daemon.api}/`);
    const frame = page.frameLocator('#frame');
    await expect(frame.locator('h1')).toHaveText('Design that ships.');
    await expect(frame.locator('.card')).toHaveCount(3);
    await expect(frame.locator('footer')).toContainText('rendered live by Astro');
    // Served where it is rather than redirected, so the frame still holds the token it was opened with.
    const framed = page.frames().find((f) => f.url().startsWith(`http://framed.localhost:${daemon.port}/`));
    expect(framed?.url()).toContain(`sl_token=${token}`);
  });

  test('a request with neither token nor cookie is refused', async ({ browser }) => {
    const context = await browser.newContext();
    const page = await context.newPage();
    const root = await page.goto(`http://framed.localhost:${daemon.port}/`);
    expect(root?.status()).toBe(403);
    const module = await page.goto(`http://framed.localhost:${daemon.port}/__sl/m/src/pages/index.astro?v=1`);
    expect(module?.status()).toBe(403);
    const wrong = await page.goto(`http://framed.localhost:${daemon.port}/?sl_token=${'0'.repeat(32)}`);
    expect(wrong?.status()).toBe(403);
    await context.close();
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
