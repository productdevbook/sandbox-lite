import type { Page } from '@playwright/test';

import { expect, test } from './fixtures';

// A marker on `window` is how a swap is told from a reload: only a navigation clears it.
async function mark(page: Page): Promise<void> {
  await page.evaluate(() => ((window as any).__slMarker = 'kept'));
}

function marker(page: Page): Promise<string | undefined> {
  return page.evaluate(() => (window as any).__slMarker);
}

test('a written component shows up in the open preview within 3 s', async ({ daemon, page }) => {
  const site = await daemon.tenant('live', 'starter');
  await page.goto(site.url('/'));
  await expect(page.locator('footer')).toContainText('rendered live by Astro');

  const footer = await site.read('src/components/Footer.astro');
  const edited = footer.replace('rendered live by', 'edited by e2e and rendered live by');
  expect(edited).not.toBe(footer);
  await site.write('src/components/Footer.astro', edited);

  await expect(page.locator('footer')).toContainText('edited by e2e and rendered live by Astro', { timeout: 3_000 });
  await expect(page).toHaveTitle('Home · Lumen Studio');
});

test('a stylesheet swaps in place, without a reload', async ({ daemon, page }) => {
  const site = await daemon.tenant('live-css', 'starter');
  await page.goto(site.url('/'));
  await expect(page.locator('html')).toHaveCSS('background-color', 'rgb(251, 250, 247)');
  await mark(page);

  // tokens.css is not a module of its own: global.css reaches it through an `@import`.
  const tokens = await site.read('src/styles/tokens.css');
  const edited = tokens.replace('--bg: #fbfaf7;', '--bg: #ff0000;');
  expect(edited).not.toBe(tokens);
  await site.write('src/styles/tokens.css', edited);

  await expect(page.locator('html')).toHaveCSS('background-color', 'rgb(255, 0, 0)', { timeout: 3_000 });
  expect(await marker(page), 'the page was not reloaded').toBe('kept');
});

test("a component's style block swaps in place, without a reload", async ({ daemon, page }) => {
  const site = await daemon.tenant('live-style', 'starter');
  await page.goto(site.url('/'));
  const card = page.locator('article.card').first();
  await expect(card).toHaveCSS('background-color', 'rgb(255, 255, 255)');
  await mark(page);

  const source = await site.read('src/components/Card.astro');
  const edited = source.replace('.card { background: var(--card);', '.card { background: rgb(0, 128, 0);');
  expect(edited).not.toBe(source);
  await site.write('src/components/Card.astro', edited);

  await expect(card).toHaveCSS('background-color', 'rgb(0, 128, 0)', { timeout: 3_000 });
  expect(await marker(page), 'the page was not reloaded').toBe('kept');
});

test('an edit to the frontmatter still reloads', async ({ daemon, page }) => {
  const site = await daemon.tenant('live-module', 'starter');
  await page.goto(site.url('/'));
  await expect(page.locator('h1')).toHaveText('Design that ships.');
  await mark(page);

  const source = await site.read('src/pages/index.astro');
  const edited = source.replace('const headline = "Design that ships.";', 'const headline = "Design that reloads.";');
  expect(edited).not.toBe(source);
  await site.write('src/pages/index.astro', edited);

  await expect(page.locator('h1')).toHaveText('Design that reloads.', { timeout: 5_000 });
  expect(await marker(page), 'the page was reloaded').toBeUndefined();
});
