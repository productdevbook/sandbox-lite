import { crc32, deflateSync } from 'node:zlib';

import type { Page } from '@playwright/test';

import { type Daemon, startDaemon } from './daemon';
import { expect, test } from './fixtures';

// A real 1×1 PNG, built here rather than committed: what the upload has to carry is bytes that
// survive the round trip, and a fixture in the repository would only prove the file was read.
function pixelPng(): Buffer {
  const chunk = (type: string, body: Buffer): Buffer => {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(body.length);
    const typed = Buffer.concat([Buffer.from(type, 'ascii'), body]);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(typed));
    return Buffer.concat([length, typed, crc]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(1, 0);
  ihdr.writeUInt32BE(1, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 2; // truecolour
  const idat = deflateSync(Buffer.from([0, 0x6e, 0xa8, 0xfe]));
  const signature = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  return Buffer.concat([signature, chunk('IHDR', ihdr), chunk('IDAT', idat), chunk('IEND', Buffer.alloc(0))]);
}

const row = (path: string) => `#files [data-path="${path}"]`;

// The picker is a hidden <input type=file> behind the upload button, which is how a person reaches it.
async function pick(page: Page, file: { name: string; mimeType: string; buffer: Buffer }): Promise<void> {
  const chooser = page.waitForEvent('filechooser');
  await page.click('#uploadFile');
  await (await chooser).setFiles(file);
}

test('a file is created, previewed, renamed, uploaded to and deleted from the editor', async ({ daemon, page }) => {
  const site = await daemon.tenant('editor-files', 'starter');
  await page.goto(`${daemon.api}/`);
  await page.selectOption('#tenant', 'editor-files');
  await expect(page.locator(row('src/pages/index.astro'))).toBeVisible();
  // grouped by directory: the row shows the name and the group shows the path it is under
  await expect(page.locator(row('src/pages/index.astro'))).toHaveText('index.astro');
  await expect(page.locator('#files details[data-dir="src/pages"] summary')).toContainText('src/pages/');

  await page.click('#newFile');
  await page.fill('#pathDlgInput', 'src/pages/e2e-new.astro');
  await page.click('#pathDlgOk');
  await expect(page.locator(row('src/pages/e2e-new.astro'))).toHaveText('e2e-new.astro');
  await expect(page.locator('#fileMsg')).toHaveText('created src/pages/e2e-new.astro');
  await expect(page.locator('#editor')).toHaveValue(/^---\nconst title = "e2e-new";\n---\n/);

  await page.fill('#path', '/e2e-new');
  await page.click('#go');
  await expect(page.frameLocator('#frame').locator('h1')).toHaveText('e2e-new', { timeout: 15_000 });
  // back to a page that outlives the rename: the preview reloads on every write, and a frame left
  // on the old path would fetch a route that no longer exists.
  await page.fill('#path', '/');
  await page.click('#go');
  await expect(page.frameLocator('#frame').locator('h1')).toHaveText('Design that ships.');

  await page.click('#renameFile');
  await expect(page.locator('#pathDlgNote')).toContainText('two steps');
  await page.fill('#pathDlgInput', 'src/pages/e2e-renamed.astro');
  await page.click('#pathDlgOk');
  await expect(page.locator(row('src/pages/e2e-renamed.astro'))).toBeVisible();
  await expect(page.locator(row('src/pages/e2e-new.astro'))).toHaveCount(0);
  await expect(page.locator('#fileMsg')).toContainText('renamed src/pages/e2e-new.astro → src/pages/e2e-renamed.astro');

  await page.fill('#path', '/e2e-renamed');
  await page.click('#go');
  await expect(page.frameLocator('#frame').locator('h1'), 'the content moved with the path').toHaveText('e2e-new');
  await page.fill('#path', '/');
  await page.click('#go');

  const png = pixelPng();
  await pick(page, { name: 'e2e-pixel.png', mimeType: 'image/png', buffer: png });
  await expect(page.locator('#fileMsg')).toHaveText('uploaded public/e2e-pixel.png');
  await expect(page.locator(row('public/e2e-pixel.png'))).toHaveText('e2e-pixel.png');
  await expect(page.locator(row('public/e2e-pixel.png'))).toHaveAttribute('title', `public/e2e-pixel.png · ${png.length} bytes`);
  const served = await fetch(site.url('/e2e-pixel.png'));
  expect(served.status).toBe(200);
  expect(served.headers.get('content-type')).toBe('image/png');
  expect(Buffer.from(await served.arrayBuffer()).equals(png), 'the bytes the preview serves are the bytes uploaded').toBe(true);

  await page.click(row('public/e2e-pixel.png'));
  await expect(page.locator('#fileName')).toHaveText('public/e2e-pixel.png');
  await page.click('#deleteFile');
  await expect(page.locator('#delDlgPath')).toHaveText('public/e2e-pixel.png');
  await page.click('#delDlg button[value="ok"]');
  await expect(page.locator(row('public/e2e-pixel.png'))).toHaveCount(0);
  await expect(page.locator('#fileMsg')).toHaveText('deleted public/e2e-pixel.png');

  await page.click(row('src/pages/e2e-renamed.astro'));
  await page.click('#deleteFile');
  await expect(page.locator('#delDlgPath')).toHaveText('src/pages/e2e-renamed.astro');
  await page.click('#delDlg button[value="ok"]');
  await expect(page.locator(row('src/pages/e2e-renamed.astro'))).toHaveCount(0);
  await expect(page.locator('#fileName'), 'the deleted file is no longer open').toHaveText('no file');
  // every step is in the list: what was created and renamed is gone, the base project is untouched
  await expect(page.locator(row('src/pages/index.astro'))).toBeVisible();
  await expect(page.locator(row('public/favicon.svg'))).toBeVisible();
});

test('a dropped file lands in the directory it was dropped on, and in public/ otherwise', async ({ daemon, page }) => {
  await daemon.tenant('editor-drop', 'starter');
  await page.goto(`${daemon.api}/`);
  await page.selectOption('#tenant', 'editor-drop');
  await expect(page.locator(row('src/styles/tokens.css'))).toBeVisible();

  const drop = (selector: string, name: string, text: string) =>
    page.evaluate(
      ({ selector, name, text }) => {
        const target = document.querySelector(selector)!;
        const data = new DataTransfer();
        data.items.add(new File([text], name, { type: 'text/plain' }));
        for (const type of ['dragover', 'drop']) {
          target.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: data }));
        }
      },
      { selector, name, text },
    );

  await drop(row('src/styles/tokens.css'), 'dropped.css', '.dropped { color: red }\n');
  await expect(page.locator(row('src/styles/dropped.css'))).toBeVisible();
  await expect(page.locator('#fileMsg')).toHaveText('uploaded src/styles/dropped.css');

  await drop('#files', 'dropped.txt', 'dropped on the list itself\n');
  await expect(page.locator(row('public/dropped.txt'))).toBeVisible();
  await expect(page.locator('#fileMsg')).toHaveText('uploaded public/dropped.txt');
});

test('a file named like markup is text in the list, in the dialog and in the message', async ({ daemon, page }) => {
  await daemon.tenant('editor-hostile', 'starter');
  await page.goto(`${daemon.api}/`);
  await page.selectOption('#tenant', 'editor-hostile');
  const hostile = '<img src=x onerror=alert(1)>.astro';

  await page.click('#newFile');
  await page.fill('#pathDlgInput', hostile);
  await page.click('#pathDlgOk');
  await expect(page.locator(row(hostile))).toHaveText(hostile);
  await expect(page.locator('#fileMsg')).toHaveText(`created ${hostile}`);
  await expect(page.locator('#files img'), 'the name is a label, not markup').toHaveCount(0);

  await page.click('#deleteFile');
  await expect(page.locator('#delDlgPath')).toHaveText(hostile);
  await expect(page.locator('#delDlg img')).toHaveCount(0);
  await page.click('#delDlg button[value="ok"]');
  await expect(page.locator(row(hostile))).toHaveCount(0);
  await expect(page.locator('#fileMsg')).toHaveText(`deleted ${hostile}`);
});

test("a path the daemon refuses is reported in the daemon's own words", async ({ daemon, page, browserLog }) => {
  await daemon.tenant('editor-badpath', 'starter');
  await page.goto(`${daemon.api}/`);
  await page.selectOption('#tenant', 'editor-badpath');

  await page.click('#newFile');
  await page.fill('#pathDlgInput', 'src/pages/a\\b.astro');
  await page.click('#pathDlgOk');
  // `bad path` is what src/store.rs::clean_path makes the API say; the editor adds only the context.
  await expect(page.locator('#fileMsg')).toHaveText('src/pages/a\\b.astro was not created: bad path');
  await expect(page.locator('#fileMsg')).toHaveClass(/bad/);
  // not row(): a backslash inside a CSS attribute selector is an escape, so the name is matched as text
  await expect(page.locator('#files')).not.toContainText('a\\b.astro');

  // A path the URL parser would rewrite never reaches the daemon, and saying "bad path" there would
  // be putting words in its mouth.
  await page.click('#newFile');
  await page.fill('#pathDlgInput', 'src/pages/../../escape.astro');
  await page.click('#pathDlgOk');
  await expect(page.locator('#fileMsg')).toContainText('the browser rewrites');

  const { failedRequests } = browserLog.take();
  expect(failedRequests.filter((r) => r.includes(': 400')), 'the refusal was a 400 from the daemon').toHaveLength(1);
});

test.describe('tenant quota', () => {
  let quotaDaemon: Daemon;

  test.beforeAll(async () => {
    quotaDaemon = await startDaemon(['--tenant-quota-mb', '1']);
    await quotaDaemon.tenant('editor-quota', 'starter');
  });

  test.afterAll(async () => {
    await quotaDaemon?.stop();
  });

  test('an upload past the quota reports the 413 the daemon sent', async ({ page, browserLog }) => {
    await page.goto(`${quotaDaemon.api}/`);
    await page.selectOption('#tenant', 'editor-quota');
    await expect(page.locator(row('src/pages/index.astro'))).toBeVisible();

    await pick(page, { name: 'too-big.bin', mimeType: 'application/octet-stream', buffer: Buffer.alloc(2 << 20, 7) });
    await expect(page.locator('#fileMsg')).toContainText('public/too-big.bin was not uploaded: tenant quota exceeded');
    await expect(page.locator('#fileMsg')).toContainText('--tenant-quota-mb');
    await expect(page.locator(row('public/too-big.bin'))).toHaveCount(0);

    const { failedRequests } = browserLog.take();
    expect(failedRequests.filter((r) => r.includes(': 413'))).toHaveLength(1);
  });
});
