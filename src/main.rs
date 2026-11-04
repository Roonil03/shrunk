use std::env;
use std::fmt::Display;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Instant;

use shrunk::gen::{generate, Pattern};
use shrunk::{Agg, Codec, Db, Predicate, Query, QueryStats};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print!("{}", help_text());
        return Ok(());
    }
    match args[0].as_str() {
        "seed" => cmd_seed(&args[1..]),
        "ingest" => cmd_ingest(&args[1..]),
        "query" => cmd_query(&args[1..]),
        "stats" => cmd_stats(&args[1..]),
        "bench" => cmd_bench(&args[1..]),
        "codec-bench" => cmd_codec_bench(&args[1..]),
        "demo" => cmd_demo(&args[1..]),
        other => Err(format!("unknown command {other:?}\n\n{}", help_text())),
    }
}

fn help_text() -> &'static str {
    "shrunk - a tiny time series database with queryable compression

usage:
  shrunk seed <db> --series NAME [--pattern cpu|counter|constant|noise|saw|oscillating]
                 [--points N] [--interval MS] [--start MS] [--block-size N]
  shrunk ingest <db> --series NAME --file PATH
  shrunk query <db> --series NAME [--from MS] [--to MS] [--agg count|sum|min|max|avg]
                [--gt X|--ge X|--lt X|--le X|--eq X|--ne X]
                [--limit N] [--csv] [--explain]
  shrunk stats <db>
  shrunk bench [--points N] [--pattern NAME] [--block-size N]
  shrunk codec-bench [--points N] [--block-size N]
  shrunk demo [--points N]

notes:
  timestamps are milliseconds. time ranges are inclusive.
  values are f64. queries skip blocks using time and value bounds, and
  full-block aggregates use metadata instead of decompressing the block.
"
}

fn cmd_seed(args: &[String]) -> Result<(), String> {
    let path = db_arg(args)?;
    if path.exists() {
        return Err(format!(
            "{} already exists; choose another path or remove it",
            path.display()
        ));
    }
    let series = flag(args, "--series").unwrap_or("metric");
    let pattern_s = flag(args, "--pattern").unwrap_or("cpu");
    let pattern = Pattern::parse(pattern_s).ok_or_else(|| {
        format!("unknown pattern {pattern_s:?} (cpu, counter, constant, noise, saw, oscillating)")
    })?;
    let points: usize = number(flag(args, "--points"), "--points", 100_000)?;
    let interval: i64 = number(flag(args, "--interval"), "--interval", 1000)?;
    let start: i64 = number(flag(args, "--start"), "--start", 1_700_000_000_000)?;
    let block_size: usize = number(flag(args, "--block-size"), "--block-size", 1024)?;

    let batch = generate(pattern, points, start, interval);
    let mut db = Db::with_block_size(block_size);
    let t0 = Instant::now();
    db.write_many(series, &batch).map_err(|e| e.to_string())?;
    let ingest = t0.elapsed();
    let t1 = Instant::now();
    db.save(&path).map_err(|e| e.to_string())?;
    let save = t1.elapsed();

    println!(
        "seeded {} points into {:?} ({}, block size {}) in {:.0} ms ({}), saved in {:.0} ms",
        points,
        series,
        pattern.name(),
        db.block_size(),
        ingest.as_secs_f64() * 1000.0,
        rate(points, ingest),
        save.as_secs_f64() * 1000.0
    );
    print_summaries(&db);
    Ok(())
}

fn cmd_ingest(args: &[String]) -> Result<(), String> {
    let path = db_arg(args)?;
    let series = require_flag(args, "--series")?;
    let file = require_flag(args, "--file")?;
    let text = fs::read_to_string(file).map_err(|e| format!("read {file}: {e}"))?;

    let mut db = Db::load_or_new(&path).map_err(|e| e.to_string())?;
    let before = db.point_count();
    let t0 = Instant::now();
    let mut points = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line
            .split(|c: char| c == ',' || c == '\t' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        if fields.len() < 2 {
            continue;
        }
        let (ts, value) = match (fields[0].parse::<i64>(), fields[1].parse::<f64>()) {
            (Ok(ts), Ok(value)) => (ts, value),
            _ if lineno == 0 => continue, // header row
            _ => {
                return Err(format!(
                    "line {}: expected integer_ms,value (got {:?})",
                    lineno + 1,
                    line
                ));
            }
        };
        points.push(shrunk::Point { ts, value });
    }
    // One atomic batch: a duplicate or out-of-order line cannot leave a
    // partially ingested file behind.
    db.write_many(series, &points).map_err(|e| e.to_string())?;
    db.save(&path).map_err(|e| e.to_string())?;
    let added = db.point_count() - before;
    println!(
        "ingested {} points into {:?} in {:.0} ms ({})",
        added,
        series,
        t0.elapsed().as_secs_f64() * 1000.0,
        rate(added as usize, t0.elapsed())
    );
    print_summaries(&db);
    Ok(())
}

fn cmd_query(args: &[String]) -> Result<(), String> {
    let path = db_arg(args)?;
    let series = require_flag(args, "--series")?;
    let from: i64 = number(flag(args, "--from"), "--from", i64::MIN)?;
    let to: i64 = number(flag(args, "--to"), "--to", i64::MAX)?;
    let limit: Option<usize> = match flag(args, "--limit") {
        Some(v) => Some(v.parse().map_err(|_| format!("invalid --limit {v:?}"))?),
        None => None,
    };
    let explain = args.iter().any(|a| a == "--explain");
    let csv = args.iter().any(|a| a == "--csv");

    let db = Db::open(&path).map_err(|e| e.to_string())?;
    let pred = parse_predicate(args)?;

    if let Some(agg_s) = flag(args, "--agg") {
        let agg = Agg::parse(agg_s)
            .ok_or_else(|| format!("unknown aggregate {agg_s:?} (count, sum, min, max, avg)"))?;
        let res = db
            .aggregate(series, from, to, agg, pred)
            .map_err(|e| e.to_string())?;
        match res.value {
            Some(v) => println!("{v:.6}"),
            None => println!("null"),
        }
        if explain {
            print_query_stats(&res.stats);
        }
        return Ok(());
    }

    let mut q = Query::new(series, from, to);
    if let Some(p) = pred {
        q = q.filter(p);
    }
    if let Some(n) = limit {
        q = q.limit(n);
    }
    let res = db.query(&q).map_err(|e| e.to_string())?;

    if csv {
        println!("timestamp,value");
    }
    for p in &res.points {
        if csv {
            println!("{},{}", p.ts, p.value);
        } else {
            println!("{} {}", p.ts, p.value);
        }
    }
    if explain {
        print_query_stats(&res.stats);
    }
    Ok(())
}

fn cmd_stats(args: &[String]) -> Result<(), String> {
    let path = db_arg(args)?;
    let db = Db::open(&path).map_err(|e| e.to_string())?;
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("database: {} ({})", path.display(), human_bytes(size));
    print_summaries(&db);
    Ok(())
}

fn cmd_bench(args: &[String]) -> Result<(), String> {
    let points: usize = number(flag(args, "--points"), "--points", 1_000_000)?;
    let pattern_s = flag(args, "--pattern").unwrap_or("cpu");
    let pattern =
        Pattern::parse(pattern_s).ok_or_else(|| format!("unknown pattern {pattern_s:?}"))?;
    let block_size: usize = number(flag(args, "--block-size"), "--block-size", 1024)?;
    let series = "bench";
    let start = 1_700_000_000_000i64;
    let interval = 1000i64;

    let path = temp_path("bench");
    let _ = fs::remove_file(&path);

    let batch = generate(pattern, points, start, interval);
    let mut db = Db::with_block_size(block_size);
    let t0 = Instant::now();
    db.write_many(series, &batch).map_err(|e| e.to_string())?;
    let ingest = t0.elapsed();
    let t1 = Instant::now();
    db.save(&path).map_err(|e| e.to_string())?;
    let save = t1.elapsed();
    let file_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let last_ts = start + (points as i64 - 1) * interval;

    let t2 = Instant::now();
    let db = Db::open(&path).map_err(|e| e.to_string())?;
    let load = t2.elapsed();

    let t3 = Instant::now();
    let scan = db
        .query(&Query::new(series, start, last_ts))
        .map_err(|e| e.to_string())?;
    let scan_time = t3.elapsed();

    let t4 = Instant::now();
    let agg = db
        .aggregate(series, start, last_ts, Agg::Avg, None)
        .map_err(|e| e.to_string())?;
    let agg_time = t4.elapsed();

    // A 1% time window with a high-value predicate: most blocks should be
    // rejected before decoding.
    let window = (points as i64 / 100).max(1) * interval;
    let t5 = Instant::now();
    let filtered = db
        .query(&Query::new(series, start, start + window).filter(Predicate::Gt(90.0)))
        .map_err(|e| e.to_string())?;
    let filter_time = t5.elapsed();

    println!(
        "shrunk bench: {points} points, pattern {}, block size {block_size}",
        pattern.name()
    );
    println!(
        "ingest     {:>8.1} ms  ({} )",
        ingest.as_secs_f64() * 1000.0,
        rate(points, ingest)
    );
    println!(
        "save       {:>8.1} ms  ({} on disk)",
        save.as_secs_f64() * 1000.0,
        human_bytes(file_size)
    );
    println!("open       {:>8.1} ms", load.as_secs_f64() * 1000.0);
    println!(
        "scan all   {:>8.1} ms  ({} points returned)",
        scan_time.as_secs_f64() * 1000.0,
        scan.points.len()
    );
    println!(
        "avg all    {:>8.1} ms  (value {:?}, {} blocks answered from metadata)",
        agg_time.as_secs_f64() * 1000.0,
        agg.value,
        agg.stats.fast_blocks
    );
    println!(
        "filtered   {:>8.1} ms  ({} points, {} blocks skipped of {})",
        filter_time.as_secs_f64() * 1000.0,
        filtered.points.len(),
        filtered.stats.compression_skips(),
        filtered.stats.blocks_total
    );
    print_summaries(&db);
    let _ = fs::remove_file(&path);
    Ok(())
}

fn cmd_codec_bench(args: &[String]) -> Result<(), String> {
    let points: usize = number(flag(args, "--points"), "--points", 1_000_000)?;
    let block_size: usize = number(flag(args, "--block-size"), "--block-size", 1024)?;
    let start = 1_700_000_000_000i64;
    let interval = 1000i64;
    let patterns = [
        Pattern::Cpu,
        Pattern::Counter,
        Pattern::Constant,
        Pattern::Noise,
        Pattern::Saw,
        Pattern::Oscillating,
    ];

    println!(
        "codec-bench: {points} points per pattern, block size {block_size}; payload bytes only"
    );
    println!(
        "{:<10} {:<14} {:>9} {:>8} {:>11} {:>11}",
        "pattern", "codec", "b/pt", "ratio", "encode ns/pt", "decode ns/pt"
    );

    for pattern in patterns {
        let batch = generate(pattern, points, start, interval);
        let n = batch.len();
        // (name, bits/point, encode ns/point, decode ns/point)
        let mut rows: Vec<(&'static str, f64, f64, f64)> = Vec::new();
        for codec in Codec::CANDIDATES {
            let t0 = Instant::now();
            let mut encoded: Vec<Vec<u8>> = Vec::new();
            let mut applicable = true;
            for chunk in batch.chunks(block_size) {
                match codec.encode(chunk) {
                    Some(bytes) => encoded.push(bytes),
                    None => {
                        applicable = false;
                        break;
                    }
                }
            }
            let encode = t0.elapsed();
            if !applicable {
                continue;
            }
            let total: usize = encoded.iter().map(|b| b.len()).sum();

            let t1 = Instant::now();
            let mut decoded = 0usize;
            for (chunk, bytes) in batch.chunks(block_size).zip(&encoded) {
                match codec.decode(bytes, chunk.len()) {
                    Ok(points) => decoded += points.len(),
                    Err(e) => return Err(format!("{} decode failed: {e}", codec.name())),
                }
            }
            let decode = t1.elapsed();
            std::hint::black_box(decoded);

            let bits = (total * 8) as f64 / n as f64;
            rows.push((
                codec.name(),
                bits,
                encode.as_secs_f64() * 1e9 / n as f64,
                decode.as_secs_f64() * 1e9 / n as f64,
            ));
        }
        let best_bits = rows.iter().map(|r| r.1).fold(f64::INFINITY, f64::min);
        for (name, bits, encode_ns, decode_ns) in rows {
            let marker = if (bits - best_bits).abs() < 1e-9 {
                " *"
            } else {
                ""
            };
            println!(
                "{:<10} {:<14} {:>9.2} {:>7.1}x {:>11.1} {:>11.1}{}",
                pattern.name(),
                name,
                bits,
                128.0 / bits,
                encode_ns,
                decode_ns,
                marker
            );
        }
    }
    Ok(())
}

fn cmd_demo(args: &[String]) -> Result<(), String> {
    let points: usize = number(flag(args, "--points"), "--points", 200_000)?;
    let path = temp_path("demo");
    let _ = fs::remove_file(&path);

    let start = 1_700_000_000_000i64;
    let interval = 1000i64;
    let specs = [
        ("cpu.load", Pattern::Cpu),
        ("requests.total", Pattern::Counter),
        ("thermostat.setpoint", Pattern::Constant),
    ];

    let mut db = Db::with_block_size(1024);
    let t0 = Instant::now();
    for (name, pattern) in specs {
        let batch = generate(pattern, points, start, interval);
        db.write_many(name, &batch).map_err(|e| e.to_string())?;
    }
    let ingest = t0.elapsed();
    db.save(&path).map_err(|e| e.to_string())?;
    let file_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let db = Db::open(&path).map_err(|e| e.to_string())?;

    println!(
        "seeded {} series x {} points with {:?} in {:.0} ms; file {}",
        specs.len(),
        points,
        specs[1].1,
        ingest.as_secs_f64() * 1000.0,
        human_bytes(file_size)
    );
    print_summaries(&db);

    let last_ts = start + (points as i64 - 1) * interval;
    let avg = db
        .aggregate("cpu.load", start, last_ts, Agg::Avg, None)
        .map_err(|e| e.to_string())?;
    println!(
        "\navg(cpu.load) over all points = {:.4} ({} of {} blocks read from metadata)",
        avg.value.unwrap_or(f64::NAN),
        avg.stats.fast_blocks,
        avg.stats.blocks_total
    );

    let window_start = start + (points as i64 * 9 / 10) * interval;
    let window_end = window_start + 299_000;
    let res = db
        .query(&Query::new("cpu.load", window_start, window_end))
        .map_err(|e| e.to_string())?;
    println!(
        "query cpu.load over 300 s: {} points returned",
        res.points.len()
    );
    print_query_stats(&res.stats);

    let spikes = db
        .query(
            &Query::new("cpu.load", start, last_ts)
                .filter(Predicate::Gt(90.0))
                .limit(10),
        )
        .map_err(|e| e.to_string())?;
    println!(
        "query whole series with value > 90 (limit 10): {} spike points returned",
        spikes.points.len()
    );
    print_query_stats(&spikes.stats);

    let cnt = db
        .aggregate("cpu.load", window_start, window_end, Agg::Count, None)
        .map_err(|e| e.to_string())?;
    println!(
        "count over the same 300 s = {:?} ({} of {} blocks answered from metadata)",
        cnt.value, cnt.stats.fast_blocks, cnt.stats.blocks_total
    );

    let _ = fs::remove_file(&path);
    Ok(())
}

fn print_summaries(db: &Db) {
    println!(
        "\n{:<20} {:>10} {:>8} {:>11} {:>11} {:>8} {:>8}  codecs",
        "series", "points", "blocks", "stored", "raw", "ratio", "bits/pt"
    );
    let mut total_points = 0u64;
    let mut total_stored = 0usize;
    let mut total_raw = 0u64;
    for s in db.summaries() {
        total_points += s.points;
        total_stored += s.stored_bytes;
        total_raw += s.raw_bytes;
        let codecs = s
            .codecs
            .iter()
            .map(|c| format!("{} {}b/{}B", c.codec, c.blocks, c.bytes))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{:<20} {:>10} {:>8} {:>11} {:>11} {:>7.1}x {:>8.2}  {}",
            s.name,
            s.points,
            s.blocks,
            human_bytes(s.stored_bytes as u64),
            human_bytes(s.raw_bytes),
            s.ratio(),
            s.bits_per_point(),
            codecs
        );
    }
    let ratio = if total_stored == 0 {
        0.0
    } else {
        total_raw as f64 / total_stored as f64
    };
    println!(
        "{:<20} {:>10} {:>8} {:>11} {:>11} {:>7.1}x",
        "total",
        total_points,
        "",
        human_bytes(total_stored as u64),
        human_bytes(total_raw),
        ratio
    );
    println!();
}

fn print_query_stats(stats: &QueryStats) {
    let limit_note = if stats.blocks_skipped_limit > 0 {
        format!(", {} left unread after limit", stats.blocks_skipped_limit)
    } else {
        String::new()
    };
    eprintln!(
        "blocks: {} total, {} visited, {} decoded, {} skipped ({} time, {} value){}{}",
        stats.blocks_total,
        stats.blocks_visited,
        stats.blocks_decoded,
        stats.compression_skips(),
        stats.blocks_skipped_time,
        stats.blocks_skipped_value,
        if stats.fast_blocks > 0 {
            format!(", {} answered from metadata", stats.fast_blocks)
        } else {
            String::new()
        },
        limit_note
    );
    eprintln!(
        "read: {} points from {} bytes of compressed payload{}",
        stats.points_decoded,
        stats.bytes_decoded,
        if stats.pending_points > 0 {
            format!(
                "; {} pending points scanned uncompressed",
                stats.pending_points
            )
        } else {
            String::new()
        }
    );
}

fn parse_predicate(args: &[String]) -> Result<Option<Predicate>, String> {
    type Ctor = fn(f64) -> Predicate;
    let checks: [(&str, Ctor); 6] = [
        ("--gt", Predicate::Gt),
        ("--ge", Predicate::Ge),
        ("--lt", Predicate::Lt),
        ("--le", Predicate::Le),
        ("--eq", Predicate::Eq),
        ("--ne", Predicate::Ne),
    ];
    let mut found = None;
    for (name, make) in checks {
        if let Some(v) = flag(args, name) {
            if found.is_some() {
                return Err("use only one of --gt/--ge/--lt/--le/--eq/--ne".into());
            }
            let x: f64 = v
                .parse()
                .map_err(|_| format!("invalid {name} value {v:?}"))?;
            found = Some(make(x));
        }
    }
    Ok(found)
}

fn db_arg(args: &[String]) -> Result<PathBuf, String> {
    args.first()
        .filter(|s| !s.starts_with("--"))
        .map(PathBuf::from)
        .ok_or_else(|| "missing database path".to_string())
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn require_flag<'a>(args: &'a [String], name: &str) -> Result<&'a str, String> {
    flag(args, name).ok_or_else(|| format!("missing {name}"))
}

fn number<T>(value: Option<&str>, name: &str, default: T) -> Result<T, String>
where
    T: FromStr,
    T::Err: Display,
{
    match value {
        None => Ok(default),
        Some(s) => s.parse().map_err(|e| format!("invalid {name} {s:?}: {e}")),
    }
}

fn rate(points: usize, elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs_f64().max(1e-9);
    let per_sec = points as f64 / secs;
    if per_sec >= 1_000_000.0 {
        format!("{:.2}M points/s", per_sec / 1_000_000.0)
    } else {
        format!("{:.0}k points/s", per_sec / 1000.0)
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn temp_path(tag: &str) -> PathBuf {
    let mut p = env::temp_dir();
    p.push(format!("shrunk-{tag}-{}.db", std::process::id()));
    p
}
