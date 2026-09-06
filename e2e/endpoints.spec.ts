import { expect, test } from './fixtures';

test.describe('starter endpoints', () => {
  test('/rss.xml', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-rss', 'starter');
    await page.goto(site.url('/rss.xml'));
    await expect(page).toHaveTitle('src/pages/rss.xml.ts');
    await expect(page.locator('h1')).toHaveText('200');
    await expect(page.locator('p b')).toHaveText('application/xml; charset=utf-8');
    const body = page.locator('pre');
    await expect(body).toContainText('<rss version="2.0">');
    await expect(body).toContainText('<title>Writing journal posts in MDX</title>');
    await expect(body).toContainText('<title>Why we render previews in the browser</title>');
    await expect(body).toContainText('<title>Launching the journal</title>');
    await expect(body, 'the link is absolute, built from the request origin').toContainText(`<link>${site.url('/blog/hello-world')}</link>`);
  });

  test('/api/products.json', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-api', 'starter');
    await page.goto(site.url('/api/products.json'));
    await expect(page).toHaveTitle('src/pages/api/products.json.ts');
    await expect(page.locator('h1')).toHaveText('200');
    await expect(page.locator('p b')).toHaveText('application/json; charset=utf-8');
    const body = await page.locator('pre').textContent();
    expect(JSON.parse(body ?? '')).toEqual([
      { slug: 'brand-sprint', name: 'Brand sprint', blurb: expect.stringContaining('focused week'), price: '€4,200' },
      { slug: 'site-refresh', name: 'Site refresh', blurb: expect.stringContaining('rebuilt'), price: '€6,800' },
      { slug: 'retainer', name: 'Design retainer', blurb: expect.stringContaining('monthly block'), price: '€2,400' },
    ]);
  });

  test('a dynamic endpoint gets the props of its getStaticPaths entry', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-dynamic', 'starter');
    await site.write(
      'src/pages/api/[slug].json.ts',
      `import { products } from "@/data/products";

export function getStaticPaths() {
  return products.map((product) => ({ params: { slug: product.slug }, props: { product } }));
}

export function GET({ params, props }) {
  return new Response(JSON.stringify({ slug: params.slug, name: props.product.name }), { headers: { "content-type": "application/json" } });
}
`,
    );
    await page.goto(site.url('/api/site-refresh.json'));
    await expect(page.locator('h1')).toHaveText('200');
    const body = await page.locator('pre').textContent();
    expect(JSON.parse(body ?? '')).toEqual({ slug: 'site-refresh', name: 'Site refresh' });
  });

  test('an html response is written as a page', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-html-endpoint', 'starter');
    await site.write(
      'src/pages/api/card.ts',
      `export function GET() {
  return new Response("<!doctype html><html><head><title>From an endpoint</title></head><body><h1>Hand-written HTML</h1></body></html>", { headers: { "content-type": "text/html; charset=utf-8" } });
}
`,
    );
    await page.goto(site.url('/api/card'));
    await expect(page).toHaveTitle('From an endpoint');
    await expect(page.locator('h1')).toHaveText('Hand-written HTML');
  });

  test('a redirecting endpoint sends the browser on', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-redirect', 'starter');
    await site.write(
      'src/pages/api/latest.ts',
      `export function GET({ redirect }) {
  return redirect("/blog/writing-in-mdx", 302);
}
`,
    );
    await page.goto(site.url('/api/latest'));
    await expect(page).toHaveURL(site.url('/blog/writing-in-mdx'));
    await expect(page.locator('h1')).toHaveText('Writing journal posts in MDX');
  });

  test('an endpoint without a handler says so', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter-no-handler', 'starter');
    await site.write('src/pages/api/broken.json.ts', 'export const answer = 42;\n');
    await page.goto(site.url('/api/broken.json'));
    await expect(page.locator('h1')).toHaveText('Endpoint without a handler');
    await expect(page.locator('pre')).toContainText('src/pages/api/broken.json.ts exports no GET or ALL function');
  });
});
