import { CDN_TIMEOUT, expect, test } from './fixtures';

test.describe('starter', () => {
  test('/', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter', 'starter');
    await page.goto(site.url('/'));
    await expect(page).toHaveTitle('Home · Lumen Studio');
    await expect(page.locator('h1')).toHaveText('Design that ships.');
    await expect(page.locator('.card')).toHaveCount(3);
    // The like button is wired by a hoisted <script> that arrives after the page, so a click is retried until it counts.
    const like = page.locator('[data-like]').first();
    await expect.poll(async () => {
      await like.click();
      return like.locator('span').textContent();
    }).not.toBe('0');
  });

  test('/about', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter', 'starter');
    await page.goto(site.url('/about'));
    await expect(page).toHaveTitle('About · Lumen Studio');
    await expect(page.locator('h1')).toHaveText('About the studio');
    await expect(page.locator('main li')).toHaveText([/Mara/, /Deniz/]);
    await expect(page.locator('nav a.active')).toHaveText('About');
  });

  test('/blog', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter', 'starter');
    await page.goto(site.url('/blog'));
    await expect(page).toHaveTitle('Journal · Lumen Studio');
    await expect(page.locator('h1')).toHaveText('Journal');
    await expect(page.locator('.posts a')).toHaveText(['Why we render previews in the browser', 'Launching the journal']);
    await expect(page.locator('.posts a').first()).toHaveAttribute('href', '/blog/hello-world');
  });

  test('/blog/hello-world', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter', 'starter');
    await page.goto(site.url('/blog/hello-world'));
    await expect(page).toHaveTitle('Why we render previews in the browser · Lumen Studio');
    await expect(page.locator('h1')).toHaveText('Why we render previews in the browser');
    await expect(page.locator('.toc a')).toHaveText(['The problem', 'The trick']);
    await expect(page.locator('article pre code')).toContainText('AstroContainer.create()');
  });

  test('/docs', async ({ daemon, page }) => {
    const site = await daemon.tenant('starter', 'starter');
    await page.goto(site.url('/docs'));
    await expect(page).toHaveTitle('Editing this site · Lumen Studio');
    await expect(page.locator('h1')).toHaveText('Editing this site');
    await expect(page.locator('article h2')).toHaveText(['What you can edit', 'What happens when you save']);
    await expect(page.locator('article ol li')).toHaveCount(3);
  });
});

test.describe('tailwind', () => {
  test('/', async ({ daemon, page }) => {
    const site = await daemon.tenant('tailwind', 'tailwind');
    await page.goto(site.url('/'));
    await expect(page).toHaveTitle('Northwind Bakery');
    await expect(page.locator('h1')).toHaveText('Bread worth crossing town for.');
    await expect(page.locator('main article h2')).toHaveText(['Baked at 5am', 'Local flour', 'No waste']);
    await expect(page.locator('h1'), 'text-5xl applied by the Tailwind browser build').toHaveCSS('font-size', '48px', { timeout: CDN_TIMEOUT });
  });

  test('/menu', async ({ daemon, page }) => {
    const site = await daemon.tenant('tailwind', 'tailwind');
    await page.goto(site.url('/menu'));
    await expect(page).toHaveTitle('Menu · Northwind Bakery');
    await expect(page.locator('h1')).toHaveText('Menu');
    await expect(page.locator('main li')).toHaveCount(4);
    await expect(page.locator('h1'), 'text-4xl applied by the Tailwind browser build').toHaveCSS('font-size', '36px', { timeout: CDN_TIMEOUT });
  });
});

test.describe('react', () => {
  test('/', async ({ daemon, page }) => {
    const site = await daemon.tenant('react', 'react');
    await page.goto(site.url('/'));
    await expect(page).toHaveTitle('Islands', { timeout: CDN_TIMEOUT });
    await expect(page.locator('h1')).toHaveText('Islands in the preview');
    await expect(page.locator('.counter strong')).toHaveText('41');
    await expect(page.locator('.greeting h2')).toHaveText('Hello, static React');
    await expect(page.locator('astro-island:not([ssr])'), 'island hydrated').toHaveCount(1, { timeout: CDN_TIMEOUT });
  });
});
