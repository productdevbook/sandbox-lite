import { spawn, type ChildProcess } from 'node:child_process';
import { createServer, type AddressInfo } from 'node:net';
import path from 'node:path';

const root = path.resolve(__dirname, '..');
const binary = process.env.SANDBOX_LITE_BIN ?? path.join(root, 'target', 'release', 'sandbox-lite');

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address() as AddressInfo;
      server.close((err) => (err ? reject(err) : resolve(port)));
    });
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export class Tenant {
  constructor(
    private readonly daemon: Daemon,
    readonly id: string,
  ) {}

  url(pathname = '/'): string {
    return `http://${this.id}.localhost:${this.daemon.port}${pathname}`;
  }

  async read(file: string): Promise<string> {
    const res = await fetch(`${this.daemon.api}/api/t/${this.id}/file/${file}`);
    if (!res.ok) throw new Error(`GET ${file} on tenant ${this.id}: ${res.status} ${await res.text()}`);
    return res.text();
  }

  async write(file: string, body: string): Promise<void> {
    const res = await fetch(`${this.daemon.api}/api/t/${this.id}/file/${file}`, { method: 'PUT', body });
    if (!res.ok) throw new Error(`PUT ${file} on tenant ${this.id}: ${res.status} ${await res.text()}`);
  }
}

export class Daemon {
  readonly api: string;

  constructor(
    readonly port: number,
    private readonly child: ChildProcess,
  ) {
    this.api = `http://127.0.0.1:${port}`;
  }

  // Deleting first keeps a retried or repeated test on a pristine copy of the base.
  async tenant(id: string, base: string): Promise<Tenant> {
    await fetch(`${this.api}/api/tenants/${id}`, { method: 'DELETE' });
    const res = await fetch(`${this.api}/api/tenants`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ id, base }),
    });
    if (res.status !== 201) throw new Error(`POST /api/tenants {${id}, ${base}}: ${res.status} ${await res.text()}`);
    return new Tenant(this, id);
  }

  async stop(): Promise<void> {
    if (this.child.exitCode !== null) return;
    const exited = new Promise<void>((resolve) => this.child.once('exit', () => resolve()));
    this.child.kill();
    await Promise.race([exited, sleep(2000)]);
    if (this.child.exitCode === null) this.child.kill('SIGKILL');
  }
}

export async function startDaemon(extra: string[] = []): Promise<Daemon> {
  const port = await freePort();
  const args = ['--listen', `127.0.0.1:${port}`, '--bases', path.join(root, 'examples'), '--no-persist', ...extra];
  const child = spawn(binary, args, { stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '';
  child.stderr!.on('data', (chunk) => (stderr += chunk));
  await new Promise<void>((resolve, reject) => {
    child.once('spawn', resolve);
    child.once('error', reject);
  });
  const deadline = Date.now() + 10_000;
  for (;;) {
    if (child.exitCode !== null) throw new Error(`${binary} exited with ${child.exitCode} before answering /health\n${stderr}`);
    const healthy = await fetch(`http://127.0.0.1:${port}/health`).then((res) => res.ok, () => false);
    if (healthy) return new Daemon(port, child);
    if (Date.now() > deadline) {
      child.kill();
      throw new Error(`${binary} did not answer /health within 10 s\n${stderr}`);
    }
    await sleep(50);
  }
}
