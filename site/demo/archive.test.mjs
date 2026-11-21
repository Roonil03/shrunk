import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createConnection } from 'node:net';
import { mkdtemp, rm, access } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import { createArchive, createRestCommand, getArchiveConfig } from './archive.mjs';

// Small RESP client only for a disposable local Redis, never configured URLs.
function redisCommand(socketPath, args) {
  return new Promise((resolve, reject) => {
    const socket = createConnection(socketPath);
    let received = Buffer.alloc(0);
    function parse(offset = 0) {
      const end = received.indexOf('\r\n', offset);
      if (end < 0) return null;
      const type = String.fromCharCode(received[offset]);
      const value = received.toString('utf8', offset + 1, end);
      let next = end + 2;
      if (type === '+' || type === ':') return { value: type === ':' ? Number(value) : value, next };
      if (type === '-') throw new Error(value);
      if (type === '$') {
        const length = Number(value);
        if (length === -1) return { value: null, next };
        if (received.length < next + length + 2) return null;
        return { value: received.toString('utf8', next, next + length), next: next + length + 2 };
      }
      if (type === '*') {
        if (Number(value) === -1) return { value: null, next };
        const array = [];
        for (let i = 0; i < Number(value); i++) {
          const item = parse(next);
          if (!item) return null;
          array.push(item.value);
          next = item.next;
        }
        return { value: array, next };
      }
      throw new Error(`Unexpected RESP type ${type}`);
    }
    socket.once('connect', () => {
      const values = args.map((arg) => Buffer.from(String(arg)));
      socket.write(Buffer.concat([
        Buffer.from(`*${values.length}\r\n`),
        ...values.flatMap((value) => [Buffer.from(`$${value.length}\r\n`), value, Buffer.from('\r\n')]),
      ]));
    });
    socket.on('data', (data) => {
      received = Buffer.concat([received, data]);
      try {
        const result = parse();
        if (result) { socket.end(); resolve(result.value); }
      } catch (error) { socket.destroy(); reject(error); }
    });
    socket.once('error', reject);
    socket.setTimeout(5000, () => { socket.destroy(); reject(new Error('Local Redis test timed out')); });
  });
}

async function localRedis(t) {
  const binary = process.env.REDIS_SERVER_BIN || 'redis-server';
  const version = spawnSync(binary, ['--version'], { encoding: 'utf8' });
  assert.equal(version.status, 0, 'Install redis-server or set REDIS_SERVER_BIN to run local integration tests');
  const directory = await mkdtemp(join(tmpdir(), 'shrunk-archive-test-'));
  const socketPath = join(directory, 'redis.sock');
  const server = spawn(binary, ['--port', '0', '--unixsocket', socketPath, '--save', '', '--appendonly', 'no', '--dir', directory], { stdio: 'ignore' });
  t.after(async () => {
    server.kill('SIGTERM');
    await new Promise((resolve) => server.exitCode !== null ? resolve() : server.once('exit', resolve));
    await rm(directory, { recursive: true, force: true });
  });
  for (let attempt = 0; attempt < 100; attempt++) {
    try { await access(socketPath); await redisCommand(socketPath, ['PING']); break; }
    catch { await delay(10); }
  }
  assert.equal(await redisCommand(socketPath, ['PING']), 'PONG');
  return (args) => redisCommand(socketPath, args);
}

function batch(timestamps, value = 1.25) {
  const points = timestamps.map((ts) => ({ ts, value }));
  const raw = Buffer.alloc(points.length * 16);
  points.forEach((p, i) => { raw.writeBigInt64LE(BigInt(p.ts), i * 16); raw.writeDoubleLE(p.value, i * 16 + 8); });
  return { points, raw, compressed: Buffer.from([1, 2, 3]), codec: 'test' };
}

test('configuration, REST adapter, and disabled persistence', async () => {
  assert.equal(createArchive({ env: {} }), null);
  assert.equal(getArchiveConfig({}), null);
  assert.deepEqual(getArchiveConfig({ KV_REST_API_URL: 'https://example.invalid/', KV_REST_API_TOKEN: 'secret' }), { url: 'https://example.invalid', token: 'secret' });
  assert.throws(() => getArchiveConfig({ UPSTASH_REDIS_REST_URL: 'https://example.invalid' }));
  assert.throws(() => getArchiveConfig({ KV_REST_API_URL: 'http://example.invalid', KV_REST_API_TOKEN: 'secret' }));
  let observed;
  const command = createRestCommand({ url: 'https://example.invalid', token: 'secret', fetchImpl: async (url, options) => {
    observed = { url, options };
    return { ok: true, json: async () => ({ result: 7 }) };
  } });
  assert.equal(await command(['GET', 'key']), 7);
  assert.equal(observed.options.headers.authorization, 'Bearer secret');
  assert.deepEqual(JSON.parse(observed.options.body), ['GET', 'key']);
});

test('archive scripts against a real isolated Redis', async (t) => {
  const command = await localRedis(t);
  const make = (options = {}) => {
    const prefix = `archive-test:${randomUUID()}`;
    return { archive: createArchive({ command, prefix, cooldownMs: 0, now: () => 10_000, ...options }), prefix };
  };

  await t.test('paired bytes, bounded snapshots, retry deduplication, and replacement accounting', async () => {
    const { archive, prefix } = make();
    const lease = await archive.lease();
    assert.equal((await archive.append({ lease, ...batch([100, 200]) })).stored, true);
    assert.deepEqual(await archive.append({ lease, ...batch([100, 200]) }), { stored: false, reason: 'stale' });
    assert.deepEqual(await archive.append({ lease, ...batch([200, 300]) }), { stored: false, reason: 'stale' });
    const initial = await archive.readSnapshot({ includeTail: true });
    assert.equal(initial.rawBytes, 32);
    assert.equal(initial.compressedBytes, 3);
    assert.deepEqual(initial.points, batch([100, 200]).points);
    const replacement = batch([100, 200, 300]);
    assert.equal((await archive.append({ lease, ...replacement, replaceTailId: initial.tail.id })).stored, true);
    assert.equal((await archive.append({ lease, ...replacement, replaceTailId: initial.tail.id })).reason, 'stale-tail');
    assert.equal(await command(['HLEN', `{${prefix}}:chunks`]), 1);
    assert.equal(await command(['ZCARD', `{${prefix}}:index`]), 1);
    const snapshot = await archive.readSnapshot({ limit: 1 });
    assert.equal(snapshot.count, 3);
    assert.equal(snapshot.rawBytes, 48);
    assert.equal(snapshot.compressedBytes, 3);
    assert.deepEqual(snapshot.points, batch([300]).points);
    assert.equal('tail' in snapshot, false);
    await archive.release(lease);
  });

  await t.test('raw byte and chunk bounds prune blobs and count together', async () => {
    const { archive, prefix } = make({ maxRawBytes: 64, maxChunks: 2 });
    const lease = await archive.lease();
    for (const ts of [100, 300, 500]) await archive.append({ lease, ...batch([ts, ts + 100]) });
    const snapshot = await archive.readSnapshot();
    assert.equal(snapshot.rawBytes, 64);
    assert.equal(snapshot.count, 4);
    assert.equal(snapshot.compressedBytes, 6);
    assert.deepEqual(snapshot.points.map((p) => p.ts), [300, 400, 500, 600]);
    assert.equal(await command(['ZCARD', `{${prefix}}:index`]), 2);
    assert.equal(await command(['HLEN', `{${prefix}}:chunks`]), 2);
  });

  await t.test('observation retention prunes on reads without refreshing idle TTL', async () => {
    let current = 1000;
    const { archive, prefix } = make({ now: () => current, retentionMs: 1000 });
    const lease = await archive.lease();
    await archive.append({ lease, ...batch([500, 600]) });
    current = 1601;
    const snapshot = await archive.readSnapshot({ includeTail: true });
    assert.equal(snapshot.count, 0);
    assert.equal(snapshot.rawBytes, 0);
    assert.equal(snapshot.compressedBytes, 0);
    assert.equal(snapshot.latestTs, 600);
    assert.deepEqual(snapshot.points, []);
    assert.equal(snapshot.tail, null);
    assert.equal(await command(['HLEN', `{${prefix}}:chunks`]), 0);
  });

  await t.test('concurrent workers obtain one lease and expired leases cannot commit or release a successor', async () => {
    const { archive } = make({ leaseMs: 40, cooldownMs: 40 });
    const leases = await Promise.all(Array.from({ length: 20 }, () => archive.lease()));
    const winners = leases.filter(Boolean);
    assert.equal(winners.length, 1);
    await delay(60);
    const next = await archive.lease({ leaseMs: 1000 });
    assert.ok(next);
    assert.equal((await archive.append({ lease: winners[0], ...batch([100]) })).reason, 'lost-lease');
    assert.equal(await archive.release(winners[0]), false);
    assert.equal((await archive.append({ lease: next, ...batch([100]) })).stored, true);
    assert.equal(await archive.release(next), true);
    assert.equal(await archive.lease(), null);
    await delay(60);
    assert.ok(await archive.lease());
  });

  await t.test('concurrent retries write one batch and rejected tail mutation preserves original', async () => {
    const { archive, prefix } = make();
    const lease = await archive.lease();
    const results = await Promise.all(Array.from({ length: 10 }, () => archive.append({ lease, ...batch([100]) })));
    assert.equal(results.filter((r) => r.stored).length, 1);
    const initial = await archive.readSnapshot({ includeTail: true });
    await assert.rejects(archive.append({ lease, ...batch([100, 200], 8), replaceTailId: initial.tail.id }), /preserve existing/);
    assert.equal((await archive.readSnapshot()).count, 1);
    assert.equal(await command(['HLEN', `{${prefix}}:chunks`]), 1);
  });

  await t.test('all storage expires after inactivity and reads do not extend it', async () => {
    const { archive, prefix } = make({ retentionMs: 200 });
    const lease = await archive.lease();
    await archive.append({ lease, ...batch([9990]) });
    await delay(80);
    await archive.readSnapshot();
    await delay(160);
    for (const suffix of ['index', 'chunks', 'meta']) assert.equal(await command(['EXISTS', `{${prefix}}:${suffix}`]), 0);
    assert.equal((await archive.readSnapshot()).count, 0);
  });
});
