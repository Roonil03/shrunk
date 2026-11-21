const feed = 'https://services.swpc.noaa.gov/json/goes/primary/xrays-';
const minute = 60_000;
const blockSize = 1024;

export async function fetchReadings(cursor, now, fetchImpl = fetch) {
  // A quiet site can go a day between cron runs. Overlap the previous window
  // so late cron delivery or a missed invocation can still catch up.
  const window = cursor !== null && now - cursor > 5 * 60 * minute ? '3-day' : '6-hour';
  const response = await fetchImpl(feed + window + '.json', {
    headers: { Accept: 'application/json' },
    signal: AbortSignal.timeout(9000),
  });
  if (!response.ok) throw new Error('NOAA observations unavailable');
  const rows = await response.json();
  if (!Array.isArray(rows)) throw new Error('Unexpected NOAA data');

  const samples = new Map();
  for (const row of rows) {
    if (row?.energy !== '0.1-0.8nm' || typeof row.time_tag !== 'string'
        || !Number.isFinite(row.flux) || row.flux <= 0) continue;
    const ts = Date.parse(row.time_tag);
    if (!Number.isSafeInteger(ts) || ts > now || ts < now - 3 * 24 * 60 * minute) continue;
    if (cursor === null || ts > cursor) samples.set(ts, row.flux);
  }
  return Array.from(samples, ([ts, value]) => ({ ts, value })).sort((a, b) => a.ts - b.ts);
}

async function storeReadings(archive, lease, snapshot, fresh, encodeBatch) {
  if (!fresh.length) return;
  const tail = snapshot.tail;
  const extend = tail && tail.points.length < blockSize;
  const points = extend ? tail.points.concat(fresh) : fresh;
  for (let start = 0; start < points.length; start += blockSize) {
    const block = points.slice(start, start + blockSize);
    await archive.append({
      lease, points: block, ...encodeBatch(block),
      replaceTailId: start === 0 && extend ? tail.id : undefined,
    });
  }
}

export async function liveSnapshot(archive, encodeBatch, { now = Date.now, read = fetchReadings } = {}) {
  const lease = await archive.lease();
  let snapshot;
  let failed = false;
  try {
    snapshot = await archive.readSnapshot({ limit: 180, includeTail: Boolean(lease) });
    if (lease) {
      const fresh = await read(snapshot.latestTs, now());
      await storeReadings(archive, lease, snapshot, fresh, encodeBatch);
      if (fresh.length) snapshot = await archive.readSnapshot({ limit: 180 });
    }
  } catch (error) {
    if (!snapshot?.count) throw error;
    failed = true;
  } finally {
    // A lost connection here leaves a bounded lease which expires by itself.
    if (lease) await archive.release(lease);
  }
  const { points, count, rawBytes, compressedBytes, updatedAt, codec, latestTs } = snapshot;
  return {
    points, count, rawBytes, compressedBytes, updatedAt, codec,
    stale: failed || latestTs === null || now() - latestTs > 10 * minute,
  };
}
