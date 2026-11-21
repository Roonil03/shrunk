import { useEffect, useState } from "react";
import { CopyButton } from "@/components/ui/copy-button";
import LiveDemo from "@/components/live-demo";

const NAV = [
  { id: "live", label: "Live", href: "#live" },
  { id: "label", label: "Blocks", href: "#label" },
  { id: "queries", label: "Queries", href: "#queries" },
  { id: "codecs", label: "Codecs", href: "#codecs" },
  { id: "build", label: "Run", href: "#build" },
];

const TERMINAL = `git clone https://github.com/shrunk-db/shrunk.git
cd shrunk
cargo build --release
./target/release/shrunk demo`;

export default function App() {
  const [active, setActive] = useState(0);
  const [scrolled, setScrolled] = useState(false);

  useEffect(() => {
    const ids = NAV.map((item) => item.id);
    const update = () => {
      setScrolled(window.scrollY > 40);
      let next = 0;
      for (let i = 0; i < ids.length; i++) {
        const el = document.getElementById(ids[i]);
        if (el && el.getBoundingClientRect().top <= 96) next = i;
      }
      const atBottom =
        window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - 4;
      if (atBottom) next = ids.length - 1;
      setActive(next);
    };
    update();
    let raf = 0;
    const onScroll = () => {
      if (raf) return;
      raf = window.requestAnimationFrame(() => {
        raf = 0;
        update();
      });
    };
    window.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("resize", onScroll);
    return () => {
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("resize", onScroll);
      if (raf) window.cancelAnimationFrame(raf);
    };
  }, []);

  return (
    <>
      <header className={scrolled ? "site-head is-scrolled" : "site-head"}>
        <div className="wrap head-inner">
          <a className="brand" href="/" aria-label="shrunk home">
            <img src="/logo.svg" alt="" width={26} height={26} />
            <span>shrunk</span>
          </a>
          <nav className="site-nav" aria-label="Primary">
            {NAV.map((item, idx) => (
              <a
                key={item.id}
                href={item.href}
                className={active === idx ? "is-active" : undefined}
              >
                {item.label}
              </a>
            ))}
            <a
              className="site-nav-gh"
              href="https://github.com/shrunk-db/shrunk"
              target="_blank"
              rel="noopener noreferrer"
            >
              GitHub ↗
            </a>
          </nav>
        </div>
      </header>

      <main>
        <section className="hero">
          <video
            className="hero-sun"
            autoPlay
            loop
            muted
            playsInline
            preload="auto"
            aria-hidden="true"
            width={1024}
            height={1024}
          >
            <source src="/solar-131-av1.mp4" type='video/mp4; codecs="av01.0.08M.08"' />
            <source src="/solar-131-hevc.mp4" type='video/mp4; codecs="hvc1"' />
          </video>
          <div className="hero-scrim" aria-hidden="true" />
          <div className="wrap hero-inner">
            <div className="hero-copy">
              <p className="hero-kicker">shrunk · v3 block format · zero dependencies</p>
              <h1>
                <span className="line">Compress the readings.</span>
                <span className="line">Skip the rest.</span>
              </h1>
              <p className="lede">
                Every 1,024 readings become one sealed, compressed block paired with a
                58-byte label. Queries inspect the label before decoding a single byte.
              </p>
              <div className="actions">
                <a className="btn btn-primary" href="https://github.com/shrunk-db/shrunk">
                  Read the source
                  <svg width="15" height="15" viewBox="0 0 16 16" fill="none" aria-hidden="true">
                    <path d="M3 8h9M8.5 3.5 13 8l-4.5 4.5" stroke="currentColor" strokeWidth="1.6" strokeLinecap="square" />
                  </svg>
                </a>
                <a className="btn btn-ghost" href="#live">Live NOAA X-ray feed ↓</a>
              </div>
            </div>
          </div>
          <div className="hero-bar">
            <div className="wrap hero-bar-inner">
              <dl className="facts">
                <div className="fact">
                  <dt>Benchmark</dt>
                  <dd>6.5× smaller file</dd>
                </div>
                <div className="fact">
                  <dt>5-minute query</dt>
                  <dd>2 of 196 blocks read</dd>
                </div>
                <div className="fact">
                  <dt>Block header</dt>
                  <dd>58 bytes</dd>
                </div>
              </dl>
              <span className="hero-credit">NASA / SDO AIA 131 Å · extreme UV</span>
            </div>
          </div>
        </section>

        <LiveDemo />

        <section className="sheet" id="label">
          <div className="wrap">
            <div className="sheet-head">
              <div>
                <h2>Every block has a label.</h2>
              </div>
              <p>
                A block holds 1,024 readings and never changes. When it fills, shrunk
                writes a 58-byte header next to the compressed data. The header gives
                the block's time range, value range, count, and sum.
              </p>
            </div>

            <div className="spectable" role="table" aria-label="Block header fields">
              <div className="row head" role="row">
                <span role="columnheader">field</span>
                <span className="col-meaning" role="columnheader">meaning</span>
                <span className="bytes" role="columnheader">size</span>
              </div>
              <div className="row" role="row">
                <span className="key" role="cell">t_min, t_max</span>
                <span className="desc" role="cell">First and last timestamp</span>
                <span className="bytes" role="cell">16 B</span>
              </div>
              <div className="row" role="row">
                <span className="key" role="cell">v_min, v_max</span>
                <span className="desc" role="cell">Lowest and highest value; ignored when NaN is present</span>
                <span className="bytes" role="cell">16 B</span>
              </div>
              <div className="row" role="row">
                <span className="key" role="cell">count</span>
                <span className="desc" role="cell">Number of readings</span>
                <span className="bytes" role="cell">4 B</span>
              </div>
              <div className="row" role="row">
                <span className="key" role="cell">sum</span>
                <span className="desc" role="cell">Total of the finite values</span>
                <span className="bytes" role="cell">8 B</span>
              </div>
              <div className="row" role="row">
                <span className="key" role="cell">codec, flags</span>
                <span className="desc" role="cell">Codec id, sum flag, NaN flag</span>
                <span className="bytes" role="cell">2 B</span>
              </div>
              <div className="row" role="row">
                <span className="key" role="cell">payload_len</span>
                <span className="desc" role="cell">Size of the compressed data</span>
                <span className="bytes" role="cell">4 B</span>
              </div>
              <div className="row" role="row">
                <span className="key" role="cell">crc32 × 2</span>
                <span className="desc" role="cell">Checksums for header and data</span>
                <span className="bytes" role="cell">8 B</span>
              </div>
            </div>

            <p className="callout">
              <code>58 bytes</code> per block, checksums included. At 1,024 readings
              that is about <code>0.45 bits</code> each.
            </p>
          </div>
        </section>

        <section className="sheet" id="queries">
          <div className="wrap">
            <div className="sheet-head">
              <div>
                <h2>Queries check the label first.</h2>
              </div>
              <p>
                No index server and no column scan. shrunk walks the headers in order
                and decodes only blocks that can still contain an answer.
              </p>
            </div>

            <div className="split">
              <div className="qcard">
                <p className="q">avg(cpu.load) over all points</p>
                <h3>Use the sums and counts</h3>
                <p>
                  Each header holds the block total and its reading count, so a
                  covered block needs no reads.
                </p>
                <div className="answer"><b>196 of 196</b> blocks need no reads</div>
              </div>
              <div className="qcard">
                <p className="q">cpu.load over 300 seconds</p>
                <h3>Skip by time</h3>
                <p>
                  A binary search finds the first block that can overlap the window.
                  Blocks outside it stay closed.
                </p>
                <div className="answer"><b>2 decoded · 194 skipped</b> blocks</div>
              </div>
            </div>
          </div>
        </section>

        <section className="sheet" id="codecs">
          <div className="wrap">
            <div className="sheet-head">
              <div>
                <h2>Seven codecs. Keep the smallest.</h2>
              </div>
              <p>
                shrunk tries every codec that fits a block and keeps the shortest
                result. Flat signals and counters compress much better than noisy
                floats.
              </p>
            </div>

            <div className="table-wrap">
              <table className="codec-table">
                <thead>
                  <tr>
                    <th scope="col">series</th>
                    <th scope="col" className="col-codec">codec picked</th>
                    <th scope="col" className="num col-points">points</th>
                    <th scope="col" className="num">stored</th>
                    <th scope="col" className="num">ratio</th>
                  </tr>
                </thead>
                <tbody>
                  <tr>
                    <td className="series">cpu.load</td>
                    <td className="codec col-codec">gorilla-rle</td>
                    <td className="num col-points">200,000</td>
                    <td className="num">1.3 MiB</td>
                    <td className="num">2.3x</td>
                  </tr>
                  <tr>
                    <td className="series">requests.total</td>
                    <td className="codec col-codec">predictor</td>
                    <td className="num col-points">200,000</td>
                    <td className="num">64.2 KiB</td>
                    <td className="num">48.7x</td>
                  </tr>
                  <tr>
                    <td className="series">thermostat.setpoint</td>
                    <td className="codec col-codec">rle</td>
                    <td className="num col-points">200,000</td>
                    <td className="num">14.7 KiB</td>
                    <td className="num">212.0x</td>
                  </tr>
                  <tr className="total">
                    <td>total</td>
                    <td className="codec col-codec">all</td>
                    <td className="num col-points">600,000</td>
                    <td className="num">1.4 MiB</td>
                    <td className="num">6.5x</td>
                  </tr>
                </tbody>
              </table>
            </div>
            <p className="note">From <span style={{ color: "#2A302A" }}>shrunk demo</span>, synthetic data, release build.</p>
          </div>
        </section>

        <section className="sheet" id="build">
          <div className="wrap build">
            <div>
              <h2>Build and run it.</h2>
              <p style={{ color: "var(--muted)", maxWidth: "42ch", margin: "18px 0 0" }}>
                shrunk is a Rust crate and CLI. It has no runtime dependencies and is
                GPL-3.0.
              </p>
              <div className="links">
                <a href="https://github.com/shrunk-db/shrunk/blob/main/docs/Install.md">Install guide</a>
                <a href="https://github.com/shrunk-db/shrunk/blob/main/docs/Architecture.md">Architecture</a>
                <a href="https://github.com/shrunk-db/shrunk">Source</a>
              </div>
            </div>

            <div className="terminal" aria-label="Build commands">
              <div className="terminal-head">
                <i /><i /><i />
                <span className="sh">sh</span>
                <CopyButton code={TERMINAL} className="copy-btn" />
              </div>
              <pre>
                <span className="t-p">$</span> <span className="t-cmd">git</span> <span className="t-sub">clone</span> <span className="t-url">https://github.com/shrunk-db/shrunk.git</span>{"\n"}
                <span className="t-p">$</span> <span className="t-cmd">cd</span> shrunk{"\n"}
                <span className="t-p">$</span> <span className="t-cmd">cargo</span> <span className="t-sub">build</span> <span className="t-flag">--release</span>{"\n"}
                <span className="t-p">$</span> <span className="t-cmd">./target/release/shrunk</span> <span className="t-sub">demo</span>{"\n"}
                <span className="t-out"><span className="t-sub">seeded</span> <span className="t-num">3</span> series x <span className="t-num">200000</span> points in <span className="t-num">76 ms</span>; file <span className="t-num">1.4 MiB</span></span>{"\n"}
                <span className="t-out">…</span>{"\n"}
                <span className="t-out"><span className="t-key">blocks:</span> <span className="t-num">196</span> total, <span className="t-num">2</span> visited, <span className="t-num">2</span> decoded, <span className="t-num">194</span> skipped</span>
              </pre>
            </div>
          </div>
        </section>
      </main>

      <footer>
        <div className="wrap">
          <span>shrunk · a tiny time series database · GPL-3.0</span>
          <nav aria-label="Footer">
            <a href="https://github.com/shrunk-db/shrunk">GitHub</a>
            <a href="https://github.com/shrunk-db/shrunk/tree/main/docs">Docs</a>
            <a href="https://github.com/shrunk-db/shrunk/blob/main/LICENSE">License</a>
          </nav>
        </div>
      </footer>
    </>
  );
}
