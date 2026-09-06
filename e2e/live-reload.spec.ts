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

// Issue #62: a `module` event lost on a stream that stays open — a backgrounded tab, a network blip —
// left the page running the JS it had. The next style-only write was then correctly classified
// `style`, swapped in place, and the page went on showing new CSS over stale markup indefinitely.
// Dropping exactly one message from the open EventSource is that failure, reproduced.
test('a page that missed a module event reloads instead of swapping the next style in', async ({ daemon, page }) => {
  const site = await daemon.tenant('live-gap', 'starter');
  await page.goto(site.url('/'));
  await expect(page.locator('h1')).toHaveText('Design that ships.');
  await mark(page);

  await page.evaluate(() => {
    const es = (window as any).__sl_es as EventSource;
    const deliver = es.onmessage!.bind(es);
    (window as any).__slDropped = false;
    es.onmessage = (e: MessageEvent) => {
      if (!(window as any).__slDropped && JSON.parse(e.data).kind === 'module') {
        (window as any).__slDropped = true;
        es.onmessage = deliver;
        return;
      }
      deliver(e);
    };
  });

  const index = await site.read('src/pages/index.astro');
  const edited = index.replace('const headline = "Design that ships.";', 'const headline = "Design that self-corrects.";');
  expect(edited).not.toBe(index);
  await site.write('src/pages/index.astro', edited);

  // the event that would have reloaded the page never reached it, so the page still holds the old JS
  await expect.poll(() => page.evaluate(() => (window as any).__slDropped), { timeout: 5_000 }).toBe(true);
  expect(await marker(page), 'the dropped event reloaded the page after all').toBe('kept');
  await expect(page.locator('h1')).toHaveText('Design that ships.');

  // a style-only write follows a version this page never rendered, so it reloads rather than swapping
  const card = await site.read('src/components/Card.astro');
  const restyled = card.replace('.card { background: var(--card);', '.card { background: rgb(0, 128, 0);');
  expect(restyled).not.toBe(card);
  await site.write('src/components/Card.astro', restyled);

  await expect(page.locator('h1')).toHaveText('Design that self-corrects.', { timeout: 5_000 });
  expect(await marker(page), 'the page swapped the style in over stale markup').toBeUndefined();
  await expect(page.locator('article.card').first()).toHaveCSS('background-color', 'rgb(0, 128, 0)');
});
