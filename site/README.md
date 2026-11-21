# Website and live demo

Set the Vercel project root to `site/` and use the Vite preset. Connect an Upstash Redis database and enable the server-side `UPSTASH_REDIS_REST_URL` and `UPSTASH_REDIS_REST_TOKEN` variables for the target environment. The Vercel integration's `KV_REST_API_URL` and `KV_REST_API_TOKEN` names also work. Use a separate database for Preview so branch deployments cannot write to production data. Redeploy after updating the environment.

`GET /api/demo` reads NOAA's public GOES long-band X-ray flux observations. While the page is open, the browser polls every minute. A Redis lease and cooldown let visitors share each refresh. A daily production cron also ingests readings when nobody is visiting. The feed overlaps recent observations and can catch up from NOAA's three-day window after an interruption.

Each observation has a 64-bit timestamp and a 64-bit value. The archive stores those 16 original bytes alongside the smaller of the Shrunk Gorilla and raw payloads in a v3 block. The chart compares the original bytes against complete blocks, including their headers and checksums. Redis representation and index overhead are outside that comparison. The archive evicts the oldest paired blocks when raw data exceeds 100,000,000 bytes or the oldest observation in a block passes 30 days. Idle keys also expire.

Run `npm ci` and `npm run build` in this directory. For a local site with its API, use `vercel dev` with the preview database configured. `npm run dev` runs the static frontend. Run `npm run typecheck` and `npm test` to check the site and demo. Archive tests need a local `redis-server` executable and start an isolated instance. Website CI also opens the emitted blocks with the repository's Rust reader.
