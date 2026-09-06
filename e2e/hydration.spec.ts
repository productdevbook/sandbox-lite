import { CDN_TIMEOUT, expect, test } from './fixtures';

test('a client:load React island hydrates and handles clicks', async ({ daemon, page }) => {
  const site = await daemon.tenant('react', 'react');
  await page.goto(site.url('/'));
  const count = page.locator('.counter strong');
  await expect(count).toHaveText('41', { timeout: CDN_TIMEOUT });
  await expect(page.locator('astro-island:not([ssr])'), 'islands hydrated').toHaveCount(2, { timeout: CDN_TIMEOUT });

  await page.getByRole('button', { name: 'increment' }).click();
  await expect(count).toHaveText('42');
  await expect(page.locator('.counter')).toHaveAttribute('data-count', '42');

  await page.getByRole('button', { name: 'decrement' }).click();
  await expect(count).toHaveText('41');
});

test('an island imported from an npm package hydrates from the CDN', async ({ daemon, page }) => {
  const site = await daemon.tenant('react-npm', 'react');
  await page.goto(site.url('/'));
  const island = page.locator('.countup astro-island');
  await expect(island).toHaveAttribute('component-url', /^https:\/\/esm\.sh\/react-countup@/, { timeout: CDN_TIMEOUT });
  // react-countup renders an empty span server-side and counts up to `end` only once its own React runs.
  await expect(island.locator('span')).toHaveText('2026', { timeout: CDN_TIMEOUT });
});
