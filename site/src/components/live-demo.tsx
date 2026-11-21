import { useEffect, useId, useRef, useState } from "react";

type Point = { ts: number; value: number };
type Measurements = {
  points: Point[];
  rawBytes: number;
  compressedBytes: number;
  count: number;
  updatedAt: number | null;
  codec: string | null;
  stale: boolean;
};

const integer = new Intl.NumberFormat("en-US");
const date = new Intl.DateTimeFormat(undefined, {
  day: "2-digit", month: "short", year: "numeric",
  hour: "2-digit", minute: "2-digit", second: "2-digit",
  hourCycle: "h23", timeZoneName: "short",
});
const clock = new Intl.DateTimeFormat(undefined, {
  hour: "2-digit", minute: "2-digit", hourCycle: "h23",
});
const clockWithSeconds = new Intl.DateTimeFormat(undefined, {
  hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23",
});
const localZone = new Intl.DateTimeFormat(undefined, { timeZoneName: "short" })
  .formatToParts(new Date())
  .find(part => part.type === "timeZoneName")?.value ?? "";

function readMeasurements(value: unknown): Measurements {
  if (!value || typeof value !== "object") throw new Error("Invalid measurement response");
  const data = value as Measurements;
  if (!Array.isArray(data.points) || !data.points.every(point =>
    point && Number.isSafeInteger(point.ts) && point.ts > 0 && point.ts <= 8.64e15 &&
    Number.isFinite(point.value) && point.value > 0
  ) || ![data.rawBytes, data.compressedBytes, data.count].every(value =>
    Number.isSafeInteger(value) && value >= 0
  ) || !(data.updatedAt === null || (Number.isSafeInteger(data.updatedAt) && data.updatedAt > 0 && data.updatedAt <= 8.64e15)) ||
    !(data.codec === null || typeof data.codec === "string") || typeof data.stale !== "boolean") {
    throw new Error("Invalid measurement response");
  }
  return { ...data, points: [...data.points].sort((a, b) => a.ts - b.ts) };
}

export function FluxChart({ points }: { points: Point[] }) {
  const titleId = useId();
  const descId = useId();
  const element = useRef<SVGSVGElement>(null);
  const [width, setWidth] = useState(900);
  useEffect(() => {
    const svg = element.current;
    if (!svg) return;
    const observer = new ResizeObserver(entries => {
      setWidth(Math.max(240, entries[0].contentRect.width));
    });
    observer.observe(svg);
    return () => observer.disconnect();
  }, []);
  const first = points[0];
  const last = points[points.length - 1];
  const logs = points.map(point => Math.log10(point.value));
  const low = Math.min(...logs);
  const high = Math.max(...logs);
  const padding = Math.max((high - low) * 0.16, 0.08);
  const min = low - padding;
  const range = high + padding - min;
  const right = width - 18;
  const x = (ts: number) => 62 + (ts - first.ts) / Math.max(last.ts - first.ts, 1) * (right - 62);
  const y = (value: number) => 22 + (1 - (Math.log10(value) - min) / range) * 164;
  // A missing minute stays a gap instead of implying an observation there.
  const path = points.map((point, index) =>
    `${index === 0 || point.ts - points[index - 1].ts > 60_000 ? "M" : "L"}${x(point.ts).toFixed(2)},${y(point.value).toFixed(2)}`
  ).join(" ");

  return (
    <svg ref={element} className="flux-chart" viewBox={`0 0 ${width} 224`} role="img" aria-labelledby={`${titleId} ${descId}`}>
      <title id={titleId}>Observed GOES solar X-ray flux</title>
      <desc id={descId}>
        {points.length} observations, from {date.format(first.ts)} to {date.format(last.ts)}.
        Flux ranges from {(10 ** low).toExponential(2)} to {(10 ** high).toExponential(2)} watts per square metre.
        The vertical scale is logarithmic. Gaps indicate missing observations.
      </desc>
      {[0, 0.5, 1].map(fraction => {
        const level = 22 + fraction * 164;
        return <g key={fraction}>
          <line className="flux-grid" x1="62" x2={right} y1={level} y2={level} />
          <text x="51" y={level + 4} textAnchor="end">{(10 ** (min + range * (1 - fraction))).toExponential(1)}</text>
        </g>;
      })}
      <path className="flux-line" d={path} />
      <circle className="flux-point" cx={x(last.ts)} cy={y(last.value)} r="3" />
      <text x="62" y="213">{clock.format(first.ts)}</text>
      <text x={right} y="213" textAnchor="end">{clock.format(last.ts)}{localZone ? ` ${localZone}` : ""}</text>
    </svg>
  );
}

export default function LiveDemo() {
  const [data, setData] = useState<Measurements | null>(null);
  const [failed, setFailed] = useState(false);
  const [loading, setLoading] = useState(true);
  const [refreshed, setRefreshed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [now, setNow] = useState(Date.now());
  const [checkedAt, setCheckedAt] = useState<number | null>(null);

  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    let feedbackTimer: ReturnType<typeof setTimeout>;
    let controller: AbortController;
    let manual = attempt > 0;

    const poll = async () => {
      const isManual = manual;
      manual = false;
      const started = Date.now();
      controller = new AbortController();
      const timeout = setTimeout(() => controller.abort(), 15_000);
      setLoading(true);
      setRefreshed(false);
      try {
        const endpoint = isManual ? `/api/demo?refresh=${started}` : "/api/demo";
        const response = await fetch(endpoint, { signal: controller.signal, cache: "no-store" });
        if (!response.ok) throw new Error("Measurements unavailable");
        const result = readMeasurements(await response.json());
        if (isManual) {
          const elapsed = Date.now() - started;
          if (elapsed < 350) await new Promise(resolve => setTimeout(resolve, 350 - elapsed));
        }
        if (!disposed) {
          setData(result);
          setFailed(false);
          setCheckedAt(Date.now());
          if (isManual) {
            setRefreshed(true);
            feedbackTimer = setTimeout(() => {
              if (!disposed) setRefreshed(false);
            }, 1800);
          }
        }
      } catch {
        if (!disposed) setFailed(true);
      } finally {
        clearTimeout(timeout);
        if (!disposed) {
          setLoading(false);
          setNow(Date.now());
          timer = setTimeout(poll, 60_000);
        }
      }
    };
    void poll();
    return () => {
      disposed = true;
      clearTimeout(timer);
      clearTimeout(feedbackTimer);
      controller?.abort();
    };
  }, [attempt]);

  const points = data?.points ?? [];
  const latest = points[points.length - 1];
  const stale = !!data && (data.stale || failed || !!latest && now - latest.ts > 10 * 60_000);
  const status = !data ? failed ? "Unavailable" : "Connecting" : !latest ? "Waiting for observations" : stale ? "Delayed data" : "Live observations";
  const ratio = data && data.compressedBytes > 0 ? data.rawBytes / data.compressedBytes : null;
  const maxBytes = Math.max(data?.rawBytes ?? 0, data?.compressedBytes ?? 0, 1);

  return (
    <section className="sheet live-demo" id="live" aria-labelledby="live-title">
      <div className="wrap">
        <div className="sheet-head">
          <div>
            <h2 id="live-title">A small signal from the Sun.</h2>
          </div>
          <p>GOES measures solar X-ray flux in the 0.1–0.8 nm band. This demo fetches
            NOAA SWPC observations, stores them in shrunk-format blocks, and compares the bytes.
            It checks for new data every minute.</p>
        </div>

        <div className="live-panel">
          <div className="live-panel-head">
            <span className={`live-status${stale || failed ? " is-delayed" : ""}`} role="status">
              <i aria-hidden="true" />{status}
            </span>
            <button className="live-refresh" type="button" disabled={loading} onClick={() => setAttempt(value => value + 1)}>
              {loading ? "Checking…" : refreshed ? "Updated" : "Refresh"}
            </button>
          </div>

          {latest ? <>
            <div className="live-reading">
              <div>
                <span className="live-label">Latest X-ray flux</span>
                <p className="live-value">{latest.value.toExponential(3)} <span>W/m²</span></p>
              </div>
              <p className="live-observed">Observed <time dateTime={new Date(latest.ts).toISOString()}>{date.format(latest.ts)}</time></p>
            </div>
            {stale && <p className="live-notice">The feed is delayed. Showing the latest available observations; refreshes continue automatically.</p>}
            <figure className="live-figure">
              <FluxChart points={points} />
              <figcaption>Solar X-ray flux · W/m² · logarithmic scale · {integer.format(points.length)} plotted observations</figcaption>
            </figure>
          </> : <div className="live-empty">
            <p>{!data ? failed ? "The measurement service could not be reached." : "Fetching solar observations…" : "No solar observations have been stored yet."}</p>
            <span>{failed ? "Try Refresh, or leave this page open to retry automatically." : "The chart will appear when measurements arrive."}</span>
          </div>}

          {data && data.count > 0 && <div className="live-storage">
            <div className="live-byte-bars" aria-label="Actual storage size comparison">
              <div className="live-byte-row"><div><span>Raw readings</span><b>{integer.format(data.rawBytes)} B</b></div><span className="live-byte-track"><span style={{ width: `${data.rawBytes / maxBytes * 100}%` }} /></span></div>
              <div className="live-byte-row"><div><span>Shrunk-format blocks</span><b>{integer.format(data.compressedBytes)} B</b></div><span className="live-byte-track stored"><span style={{ width: `${data.compressedBytes / maxBytes * 100}%` }} /></span></div>
            </div>
            <dl className="live-storage-facts">
              <div><dt>Stored readings</dt><dd>{integer.format(data.count)}</dd></div>
              <div><dt>Raw / stored</dt><dd>{ratio === null ? "—" : `${ratio.toFixed(2)}×`}</dd></div>
              <div><dt>Latest block codec</dt><dd>{data.codec ?? "Not yet sealed"}</dd></div>
            </dl>
          </div>}

          <div className="live-panel-foot">
            <a href="https://www.swpc.noaa.gov/products/goes-x-ray-flux">Source: NOAA SWPC / GOES</a>
            <span>
              {data?.updatedAt ? (
                <>
                  Archive updated <time dateTime={new Date(data.updatedAt).toISOString()}>{date.format(data.updatedAt)}</time>
                  {checkedAt ? <> · Checked {clockWithSeconds.format(checkedAt)}</> : null}
                </>
              ) : "Observation timestamps are in local time"}
            </span>
          </div>
        </div>
        <p className="note">Raw size counts timestamps and values. Stored size includes block overhead. This demo selects Gorilla or raw encoding for each block. Ratios vary with the observed signal.</p>
      </div>
    </section>
  );
}
