import assert from 'node:assert/strict';
import test, { after } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const server = await createServer({
  server: { middlewareMode: true, hmr: false, ws: false },
  optimizeDeps: { noDiscovery: true, include: [] },
});
after(() => server.close());
const { FluxChart } = await server.ssrLoadModule('/src/components/live-demo.tsx');

for (const { name, offsets, commands } of [
  { name: 'continuous one-minute observations', offsets: [0, 1, 2], commands: ['M', 'L', 'L'] },
  { name: 'one missing observation', offsets: [0, 1, 3, 4], commands: ['M', 'L', 'M', 'L'] },
  { name: 'multiple missing observations', offsets: [0, 1, 4, 5], commands: ['M', 'L', 'M', 'L'] },
  { name: 'a single observation', offsets: [0], commands: ['M'] },
]) {
  test(`chart renders ${name} with the expected path segments`, () => {
    const start = Date.parse('2026-01-03T12:00:00Z');
    const points = offsets.map(offset => ({ ts: start + offset * 60_000, value: 2e-7 }));
    const markup = renderToStaticMarkup(createElement(FluxChart, { points }));
    const path = /<path class="flux-line" d="([^"]+)"/.exec(markup)?.[1];
    assert.ok(path, 'the rendered chart includes its observation path');
    assert.deepEqual(path.match(/[ML]/g), commands);
    assert.doesNotMatch(path, /NaN|Infinity/);
  });
}
