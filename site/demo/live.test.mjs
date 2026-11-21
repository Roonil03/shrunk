import assert from 'node:assert/strict';
import test from 'node:test';
import { encodeBatch } from './codec.mjs';
import { fetchReadings, liveSnapshot } from './live.mjs';

const now = Date.parse('2026-01-03T12:00:00Z');
const minute = 60_000;
const points = offsets => offsets.map(offset => ({ ts: now + offset * minute, value: 2e-7 }));
const stored = points([-3, -2]);
const snapshot = {
  count: 2, rawBytes: 32, compressedBytes: 70, updatedAt: now, codec: 'gorilla',
  latestTs: stored.at(-1).ts, points: stored, tail: { id: 'tail', points: stored },
};

test('feed selects and orders fresh long-band observations and overlaps a missed day', async () => {
  const sample = (age, flux = 2e-7, energy = '0.1-0.8nm') => ({
    energy, flux, time_tag: new Date(now - age * minute).toISOString(),
  });
  const body = [sample(0), sample(2), sample(1, 1, '0.05-0.4nm'), sample(1, -1),
    sample(1, 3e-7), sample(1), sample(-10), sample(5000), null];
  const urls = [];
  const fetchImpl = async url => { urls.push(url); return Response.json(body); };
  assert.deepEqual(await fetchReadings(now - 2 * minute, now, fetchImpl), points([-1, 0]));
  assert.deepEqual(await fetchReadings(now - 1440 * minute, now, fetchImpl), points([-2, -1, 0]));
  assert.match(urls[0], /6-hour\.json$/);
  assert.match(urls[1], /3-day\.json$/);
  await assert.rejects(fetchReadings(null, now, async () => new Response('', { status: 503 })));
});

test('refresh fills the original tail and stores subsequent observations in order', async () => {
  const fresh = Array.from({ length: 1024 }, (_, i) => ({ ts: now + i, value: i }));
  const writes = [];
  const archive = {
    lease: async () => 'lease',
    readSnapshot: async ({ includeTail }) => includeTail ? snapshot : { ...snapshot, count: 1026 },
    append: async batch => { writes.push(batch); return { stored: true }; },
    release: async token => assert.equal(token, 'lease'),
  };
  const result = await liveSnapshot(archive, encodeBatch, { now: () => now, read: async () => fresh });
  assert.equal(result.count, 1026);
  assert.equal(result.stale, false);
  assert.ok(!('tail' in result));
  assert.deepEqual(writes.map(batch => batch.points.length), [1024, 2]);
  assert.deepEqual(writes.map(batch => batch.replaceTailId), ['tail', undefined]);
  assert.deepEqual(writes.flatMap(batch => batch.points), stored.concat(fresh));
});

test('a losing writer stops at the first refused block and serves its durable snapshot', async () => {
  let attempts = 0;
  let releases = 0;
  const archive = {
    lease: async () => 'old', readSnapshot: async () => snapshot,
    append: async () => { attempts++; return { stored: false }; },
    release: async () => { releases++; },
  };
  const fresh = Array.from({ length: 1200 }, (_, i) => ({ ts: now + i, value: i }));
  const result = await liveSnapshot(archive, encodeBatch, { now: () => now, read: async () => fresh });
  assert.equal(result.stale, true);
  assert.deepEqual(result.points, stored);
  assert.equal(attempts, 1);
  assert.equal(releases, 1);
});

test('cooldown skips NOAA and old persisted observations remain visibly stale', async () => {
  const archive = { lease: async () => null, readSnapshot: async () => snapshot };
  const result = await liveSnapshot(archive, encodeBatch, {
    now: () => now + 20 * minute, read: () => assert.fail('cooldown must skip the feed'),
  });
  assert.equal(result.stale, true);
  assert.equal(result.rawBytes, 32);
});

test('a failed lease release preserves a valid snapshot', async () => {
  const archive = {
    lease: async () => 'lease', readSnapshot: async () => snapshot,
    release: async () => { throw new Error('Release connection lost'); },
  };
  const result = await liveSnapshot(archive, encodeBatch, { now: () => now, read: async () => [] });
  assert.equal(result.stale, false);
  assert.deepEqual(result.points, stored);
});

test('a failed lease release preserves the stale fallback after a refresh error', async () => {
  const archive = {
    lease: async () => 'lease', readSnapshot: async () => snapshot,
    release: async () => { throw new Error('Release connection lost'); },
  };
  const result = await liveSnapshot(archive, encodeBatch, {
    now: () => now, read: async () => { throw new Error('Feed unavailable'); },
  });
  assert.equal(result.stale, true);
  assert.deepEqual(result.points, stored);
});

test('a failed lease release does not mask an error without a durable snapshot', async () => {
  const readError = new Error('Snapshot unavailable');
  const archive = {
    lease: async () => 'lease',
    readSnapshot: async () => { throw readError; },
    release: async () => { throw new Error('Release connection lost'); },
  };
  await assert.rejects(liveSnapshot(archive, encodeBatch), error => error === readError);
});
