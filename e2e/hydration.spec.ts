import { CDN_TIMEOUT, expect, test } from './fixtures';

test('a client:load React island hydrates and handles clicks', async ({ daemon, page }) => {
  const site = await daemon.tenant('react', 'react');
  await page.goto(site.url('/'));
  const count = page.locator('.counter strong');
  await expect(count).toHaveText('41', { timeout: CDN_TIMEOUT });
  await expect(page.locator('astro-island:not([ssr])'), 'island hydrated').toHaveCount(1, { timeout: CDN_TIMEOUT });

  await page.getByRole('button', { name: 'increment' }).click();
  await expect(count).toHaveText('42');
  await expect(page.locator('.counter')).toHaveAttribute('data-count', '42');

  await page.getByRole('button', { name: 'decrement' }).click();
  await expect(count).toHaveText('41');
});
