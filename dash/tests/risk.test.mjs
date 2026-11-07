import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { createServer } from 'vite';

let server, Notice, render, risk;
before(async () => {
  server = await createServer({ root: fileURLToPath(new URL('../', import.meta.url)), server: { middlewareMode: true }, appType: 'custom', logLevel: 'error' });
  ({ default: Notice } = await server.ssrLoadModule('/src/lib/components/risk-notice.svelte'));
  ({ render } = await server.ssrLoadModule('svelte/server'));
  risk = await server.ssrLoadModule('/src/lib/risk.ts');
  const { setLanguage } = await server.ssrLoadModule('/src/lib/i18n.svelte.ts');
  setLanguage('en');
});
after(() => server?.close());

function memory() {
  const values = new Map();
  return { getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, String(value)) };
}

test('the warning stays until accepted, then never shows again in that browser', () => {
  const storage = memory();
  assert.equal(risk.riskAccepted(storage), false);
  risk.acceptRisk(storage);
  assert.equal(risk.riskAccepted(storage), true);
});

test('a browser without storage shows the warning instead of failing', () => {
  const broken = { getItem() { throw new Error('denied'); }, setItem() { throw new Error('denied'); } };
  assert.equal(risk.riskAccepted(broken), false);
  assert.doesNotThrow(() => risk.acceptRisk(broken));
  assert.equal(risk.riskAccepted(undefined), false);
});

test('a browser that blocks localStorage itself still opens the dashboard', () => {
  const original = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, get() { throw new DOMException('blocked', 'SecurityError'); } });
  try {
    assert.equal(risk.riskAccepted(), false);
    assert.doesNotThrow(() => risk.acceptRisk());
  } finally {
    if (original) Object.defineProperty(globalThis, 'localStorage', original); else delete globalThis.localStorage;
  }
});

test('the warning names the ban risk and can only be closed by accepting it', () => {
  const html = render(Notice, { props: { accept() {} } }).body;
  assert.match(html, /suspend or ban the account/);
  assert.match(html, /no warranty/);
  assert.match(html, />I understand</);
  assert.doesNotMatch(html, /aria-label="Close"/);
});
