import { expect, test } from './fixtures';

test('a compile error shows the overlay with file:line:column and the fix recovers', async ({ daemon, page, browserLog }) => {
  const site = await daemon.tenant('broken', 'starter');
  const requested: string[] = [];
  page.on('request', (request) => requested.push(request.url()));
  await page.goto(site.url('/'));
  await expect(page).toHaveTitle('Home · Lumen Studio');

  const original = await site.read('src/pages/index.astro');
  const broken = original.replace('const headline = "Design that ships.";', 'const headline = ;');
  expect(broken).not.toBe(original);
  await site.write('src/pages/index.astro', broken);

  await expect(page).toHaveTitle('Render failed');
  await expect(page.locator('h1')).toHaveText('Render failed');
  await expect(page.locator('li b')).toHaveText(/^src\/pages\/index\.astro:6:\d+$/);
  await expect(page.locator('li')).toContainText('Unexpected token');

  // Issue #54: the diagnostic comes from the failed module's own 500 body. /__sl/check compiles
  // every source file of the tenant, each taking a compile permit, and is now only the fallback.
  expect(requested.filter((url) => url.includes('/__sl/check')), 'the overlay did not sweep the project').toEqual([]);

  const { consoleErrors, failedRequests } = browserLog.take();
  expect(failedRequests.length, 'the module request fails').toBeGreaterThan(0);
  expect(consoleErrors.length, 'the failure is logged').toBeGreaterThan(0);
  for (const entry of [...failedRequests, ...consoleErrors]) {
    expect(entry, 'nothing fails except the broken module').toContain('/__sl/m/src/pages/index.astro');
  }

  await site.write('src/pages/index.astro', original);
  await expect(page).toHaveTitle('Home · Lumen Studio');
  await expect(page.locator('h1')).toHaveText('Design that ships.');
});
