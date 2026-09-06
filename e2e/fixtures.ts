import { test as base, expect, type ConsoleMessage, type Page } from '@playwright/test';
import { type Daemon, startDaemon } from './daemon';

export { expect };

// The react example renders through esm.sh and the tailwind example styles through jsdelivr.
export const CDN_TIMEOUT = 45_000;

function format(message: ConsoleMessage): string {
  const { url } = message.location();
  return url ? `${message.text()} (${url})` : message.text();
}

export class BrowserLog {
  readonly consoleErrors: string[] = [];
  readonly failedRequests: string[] = [];

  constructor(page: Page) {
    page.on('console', (message) => {
      if (message.type() === 'error') this.consoleErrors.push(format(message));
    });
    page.on('pageerror', (error) => this.consoleErrors.push(`uncaught ${error.message}`));
    page.on('requestfailed', (request) => this.failedRequests.push(`${request.method()} ${request.url()}: ${request.failure()?.errorText}`));
    page.on('response', (response) => {
      if (response.status() >= 400) this.failedRequests.push(`${response.request().method()} ${response.url()}: ${response.status()}`);
    });
  }

  take(): { consoleErrors: string[]; failedRequests: string[] } {
    const taken = { consoleErrors: [...this.consoleErrors], failedRequests: [...this.failedRequests] };
    this.consoleErrors.length = 0;
    this.failedRequests.length = 0;
    return taken;
  }

  expectClean(): void {
    expect.soft(this.consoleErrors, 'console errors').toEqual([]);
    expect.soft(this.failedRequests, 'failed requests').toEqual([]);
  }
}

export const test = base.extend<{ browserLog: BrowserLog }, { daemon: Daemon }>({
  daemon: [
    async ({}, use) => {
      const daemon = await startDaemon();
      await use(daemon);
      await daemon.stop();
    },
    { scope: 'worker' },
  ],
  browserLog: [
    async ({ page }, use) => {
      const log = new BrowserLog(page);
      await use(log);
      log.expectClean();
    },
    { auto: true },
  ],
});
