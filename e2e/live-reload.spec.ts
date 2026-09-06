import { expect, test } from './fixtures';

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
