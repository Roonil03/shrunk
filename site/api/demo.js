import { createArchive } from '../demo/archive.mjs';
import { encodeBatch } from '../demo/codec.mjs';
import { liveSnapshot } from '../demo/live.mjs';

export default async function handler(req, res) {
  res.setHeader('Cache-Control', 'no-store');
  if (req.method !== 'GET') {
    res.setHeader('Allow', 'GET');
    return res.status(405).json({ error: 'Method not allowed' });
  }
  try {
    const archive = createArchive();
    if (!archive) throw new Error('Archive unavailable');
    const snapshot = await liveSnapshot(archive, encodeBatch);
    // Share successful reads at the edge and let each browser poll for fresh data.
    if (snapshot.count) res.setHeader('Vercel-CDN-Cache-Control', 'public, s-maxage=30');
    return res.status(200).json(snapshot);
  } catch {
    res.setHeader('Retry-After', '60');
    return res.status(503).json({ error: 'Live observations are temporarily unavailable' });
  }
}
