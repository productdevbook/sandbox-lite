import { expect, test } from './fixtures';

test.describe('starter middleware', () => {
  test('a page reads what the middleware put in locals', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-locals', 'starter');
    await page.goto(site.url('/members?token=lumen'));
    await expect(page).toHaveTitle('Members · Lumen Studio');
    await expect(page.locator('h1')).toHaveText('Welcome back to Lumen Studio');
    await expect(page.locator('.locals')).toHaveAttribute('data-studio', 'Lumen Studio');
  });

  test('a middleware redirect sends the browser on before the page renders', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-gate', 'starter');
    await page.goto(site.url('/members'));
    await expect(page).toHaveURL(site.url('/about'));
    await expect(page.locator('h1')).toHaveText('About the studio');
  });

  test('an endpoint runs behind the middleware too', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-mw-endpoint', 'starter');
    await site.write(
      'src/pages/api/studio.json.ts',
      `export function GET({ locals }) {
  return new Response(JSON.stringify({ studio: locals.studio }), { headers: { "content-type": "application/json" } });
}
`,
    );
    await page.goto(site.url('/api/studio.json'));
    await expect(page.locator('h1')).toHaveText('200');
    const body = await page.locator('pre').textContent();
    expect(JSON.parse(body ?? '')).toEqual({ studio: 'Lumen Studio' });
  });

  test('middleware can change the response next() answers with', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-mw-response', 'starter');
    await site.write(
      'src/middleware.ts',
      `export const onRequest = async (context, next) => {
  const response = await next();
  const html = await response.text();
  return new Response(html.replace("Design that ships.", "Rewritten on the way out"), response);
};
`,
    );
    await page.goto(site.url('/'));
    await expect(page.locator('h1')).toHaveText('Rewritten on the way out');
  });

  test('a middleware that exports no onRequest says so', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-mw-silent', 'starter');
    await site.write('src/middleware.ts', 'export const handler = (context, next) => next();\n');
    await page.goto(site.url('/'));
    await expect(page.locator('h1')).toHaveText('Middleware without onRequest');
    await expect(page.locator('pre')).toContainText('src/middleware.ts exports no onRequest function');
  });
});
