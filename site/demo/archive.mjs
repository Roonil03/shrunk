import { randomUUID } from 'node:crypto';

export const ARCHIVE_LIMITS = Object.freeze({ maxRawBytes: 100_000_000, maxBatchPoints: 1024, maxChunks: 8192 });
const DAY = 86_400_000;

// Every blob contains both encodings. Three storage keys share one expiry and
// hash slot; Lua changes the index, blobs, and accounting as one operation.
const PRUNE = `
local function remove(id)
  local blob = redis.call('HGET', KEYS[2], id)
  if blob then
    local c = cjson.decode(blob)
    redis.call('HINCRBY', KEYS[3], 'count', -c.count)
    redis.call('HINCRBY', KEYS[3], 'rawBytes', -c.rawBytes)
    redis.call('HINCRBY', KEYS[3], 'compressedBytes', -c.compressedBytes)
    redis.call('HDEL', KEYS[2], id)
  end
  redis.call('ZREM', KEYS[1], id)
end
local function prune(cutoff, maxBytes, maxChunks)
  for i=1,${ARCHIVE_LIMITS.maxChunks + 1} do
    local first = redis.call('ZRANGE', KEYS[1], 0, 0, 'WITHSCORES')
    if #first == 0 then break end
    local bytes = tonumber(redis.call('HGET', KEYS[3], 'rawBytes') or '0')
    if tonumber(first[2]) >= cutoff and bytes <= maxBytes and redis.call('ZCARD', KEYS[1]) <= maxChunks then break end
    remove(first[1])
  end
end
`;

const SNAPSHOT = `${PRUNE}
prune(tonumber(ARGV[1]), tonumber(ARGV[2]), tonumber(ARGV[3]))
local out = {}
local total = 0
local ids = redis.call('ZREVRANGE', KEYS[1], 0, math.min(tonumber(ARGV[4]), tonumber(ARGV[3])) - 1)
for _,id in ipairs(ids) do
  local blob = redis.call('HGET', KEYS[2], id)
  if blob then
    table.insert(out, id)
    table.insert(out, blob)
    total = total + cjson.decode(blob).count
    if total >= tonumber(ARGV[4]) then break end
  end
end
return {redis.call('HGETALL', KEYS[3]), out}
`;

const APPEND = `${PRUNE}
if redis.call('GET', KEYS[4]) ~= ARGV[1] then return {'lost-lease'} end
local c = cjson.decode(ARGV[2])
local latest = tonumber(redis.call('HGET', KEYS[3], 'latestTs') or '-1')
local replace = ARGV[4]
if replace ~= '' then
  local tail = redis.call('ZREVRANGE', KEYS[1], 0, 0)
  if #tail == 0 or tail[1] ~= replace then return {'stale-tail'} end
  local old = cjson.decode(redis.call('HGET', KEYS[2], replace))
  if c.firstTs ~= old.firstTs or c.count <= old.count or c.lastTs <= latest or ARGV[8] ~= old.raw then return {'stale-tail'} end
else
  if c.firstTs <= latest then return {'stale'} end
end
if c.firstTs < tonumber(ARGV[5]) then return {'expired'} end
if replace ~= '' then remove(replace) end
redis.call('HSET', KEYS[2], ARGV[3], ARGV[2])
redis.call('ZADD', KEYS[1], c.firstTs, ARGV[3])
redis.call('HINCRBY', KEYS[3], 'count', c.count)
redis.call('HINCRBY', KEYS[3], 'rawBytes', c.rawBytes)
redis.call('HINCRBY', KEYS[3], 'compressedBytes', c.compressedBytes)
redis.call('HSET', KEYS[3], 'latestTs', c.lastTs, 'updatedAt', ARGV[9], 'codec', c.codec)
prune(tonumber(ARGV[5]), tonumber(ARGV[6]), tonumber(ARGV[7]))
for i=1,3 do redis.call('PEXPIRE', KEYS[i], ARGV[10]) end
return {'stored'}
`;

const LEASE = `
if redis.call('EXISTS', KEYS[2]) == 1 then return 0 end
return redis.call('SET', KEYS[1], ARGV[1], 'NX', 'PX', ARGV[2]) or 0
`;
const RELEASE = `
if redis.call('GET', KEYS[1]) ~= ARGV[1] then return 0 end
redis.call('DEL', KEYS[1])
if tonumber(ARGV[2]) > 0 then redis.call('SET', KEYS[2], '1', 'PX', ARGV[2]) end
return 1
`;

function integer(value, fallback, min, max, name) {
  const n = value === undefined ? fallback : Number(value);
  if (!Number.isSafeInteger(n) || n < min || n > max) throw new Error(`Invalid archive ${name}`);
  return n;
}

export function getArchiveConfig(env = process.env) {
  const url = env.UPSTASH_REDIS_REST_URL || env.KV_REST_API_URL;
  const token = env.UPSTASH_REDIS_REST_TOKEN || env.KV_REST_API_TOKEN;
  if (!url && !token) return null;
  if (!url || !token) throw new Error('Archive Redis URL and token must both be configured');
  const parsed = new URL(url);
  if (parsed.protocol !== 'https:' && !(parsed.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(parsed.hostname))) {
    throw new Error('Archive Redis requires HTTPS');
  }
  return { url: url.replace(/\/$/, ''), token };
}

export function createRestCommand({ url, token, fetchImpl = globalThis.fetch }) {
  return async (command) => {
    const response = await fetchImpl(url, {
      method: 'POST', headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
      body: JSON.stringify(command), signal: AbortSignal.timeout(12_000),
    });
    if (!response.ok) throw new Error(`Archive Redis request failed (${response.status})`);
    const body = await response.json();
    if (body.error) throw new Error(`Archive Redis: ${body.error}`);
    return body.result;
  };
}

function decodeRaw(encoded) {
  const raw = Buffer.from(encoded, 'base64');
  if (raw.length % 16) throw new Error('Corrupt archive chunk');
  const points = [];
  for (let offset = 0; offset < raw.length; offset += 16) {
    points.push({ ts: Number(raw.readBigInt64LE(offset)), value: raw.readDoubleLE(offset + 8) });
  }
  return points;
}

/** A null result means persistence is not configured. Never falls back to memory. */
export function createArchive(options = {}) {
  const { env = process.env, now = Date.now, fetchImpl, prefix = 'shrunk:demo:archive' } = options;
  const config = options.command ? null : getArchiveConfig(env);
  if (!options.command && !config) return null;
  const command = options.command || createRestCommand({ ...config, fetchImpl });
  const retentionMs = integer(options.retentionMs, 30 * DAY, 1, 366 * DAY, 'retentionMs');
  const idleMs = Math.min(retentionMs, integer(options.inactivityTtlSeconds, Math.ceil(retentionMs / 1000), 1, 366 * 86400, 'inactivityTtlSeconds') * 1000);
  const maxRawBytes = integer(options.maxRawBytes, ARCHIVE_LIMITS.maxRawBytes, 16, ARCHIVE_LIMITS.maxRawBytes, 'maxRawBytes');
  const maxChunks = integer(options.maxChunks, ARCHIVE_LIMITS.maxChunks, 1, ARCHIVE_LIMITS.maxChunks, 'maxChunks');
  const defaultLeaseMs = integer(options.leaseMs, 60_000, 1, 300_000, 'leaseMs');
  const defaultCooldownMs = integer(options.cooldownMs, 60_000, 0, 3_600_000, 'cooldownMs');
  if (!prefix || /[{}]/.test(prefix)) throw new Error('Invalid archive prefix');
  const root = `{${prefix}}`;
  const storageKeys = [`${root}:index`, `${root}:chunks`, `${root}:meta`];
  const lockKeys = [`${root}:lease`, `${root}:cooldown`];
  const evalScript = (script, keys, args) => command(['EVAL', script, keys.length, ...keys, ...args.map(String)]);

  return {
    async readSnapshot({ limit = 200, includeTail = false } = {}) {
      limit = integer(limit, 200, 1, 2048, 'snapshot limit');
      const [metadata, chunks] = await evalScript(SNAPSHOT, storageKeys, [now() - retentionMs, maxRawBytes, maxChunks, limit]);
      const meta = Object.fromEntries(Array.from({ length: metadata.length / 2 }, (_, i) => [metadata[i * 2], metadata[i * 2 + 1]]));
      const parsed = [];
      let tail = null;
      for (let i = 0; i < chunks.length; i += 2) {
        const chunk = JSON.parse(chunks[i + 1]);
        const points = decodeRaw(chunk.raw);
        if (i === 0 && includeTail) tail = { id: chunks[i], points };
        parsed.unshift(points);
      }
      return {
        points: parsed.flat().slice(-limit), count: Number(meta.count || 0),
        rawBytes: Number(meta.rawBytes || 0), compressedBytes: Number(meta.compressedBytes || 0),
        updatedAt: meta.updatedAt ? Number(meta.updatedAt) : null,
        latestTs: meta.latestTs ? Number(meta.latestTs) : null, codec: meta.codec || null,
        ...(includeTail ? { tail } : {}),
      };
    },

    async lease({ leaseMs = defaultLeaseMs } = {}) {
      leaseMs = integer(leaseMs, defaultLeaseMs, 1, 300_000, 'leaseMs');
      const token = randomUUID();
      const acquired = await evalScript(LEASE, lockKeys, [token, leaseMs]);
      return acquired ? token : null;
    },

    async release(lease, { cooldownMs = defaultCooldownMs } = {}) {
      cooldownMs = integer(cooldownMs, defaultCooldownMs, 0, 3_600_000, 'cooldownMs');
      return Boolean(await evalScript(RELEASE, lockKeys, [lease, cooldownMs]));
    },

    async append({ lease, points, raw, compressed, codec, replaceTailId = null }) {
      if (typeof lease !== 'string' || !lease) throw new Error('Archive append requires a lease');
      if (!Array.isArray(points) || !points.length || points.length > ARCHIVE_LIMITS.maxBatchPoints) throw new Error('Invalid archive point count');
      raw = Buffer.from(raw);
      compressed = Buffer.from(compressed);
      if (raw.length !== points.length * 16 || raw.length > maxRawBytes || !compressed.length || compressed.length > raw.length * 2 + 1024) throw new Error('Invalid archive byte lengths');
      if (typeof codec !== 'string' || !codec || codec.length > 100) throw new Error('Invalid archive codec');
      for (let i = 0; i < points.length; i++) {
        const p = points[i];
        if (!Number.isSafeInteger(p.ts) || p.ts < 0 || !Number.isFinite(p.value) || (i && p.ts <= points[i - 1].ts)
          || raw.readBigInt64LE(i * 16) !== BigInt(p.ts) || raw.readDoubleLE(i * 16 + 8) !== p.value) throw new Error('Invalid archive raw point');
      }
      let priorRaw = '';
      if (replaceTailId) {
        // This bounded read also verifies the immutable prefix, so a retry or a
        // competing writer cannot replace historical values accidentally.
        const prior = await command(['HGET', storageKeys[1], replaceTailId]);
        if (!prior) return { stored: false, reason: 'stale-tail' };
        const old = JSON.parse(prior);
        const oldRaw = Buffer.from(old.raw, 'base64');
        if (raw.length <= oldRaw.length || !raw.subarray(0, oldRaw.length).equals(oldRaw)) throw new Error('Tail replacement must preserve existing points');
        priorRaw = old.raw;
      }
      const chunk = {
        count: points.length, firstTs: points[0].ts, lastTs: points.at(-1).ts,
        rawBytes: raw.length, compressedBytes: compressed.length, codec,
        raw: raw.toString('base64'), compressed: compressed.toString('base64'),
      };
      const time = now();
      const [reason] = await evalScript(APPEND, [...storageKeys, lockKeys[0]], [
        lease, JSON.stringify(chunk), randomUUID(), replaceTailId || '', time - retentionMs,
        maxRawBytes, maxChunks, priorRaw, time, idleMs,
      ]);
      return { stored: reason === 'stored', reason };
    },
  };
}
