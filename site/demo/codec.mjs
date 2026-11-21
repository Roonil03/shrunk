import { Buffer } from 'node:buffer';

// A deliberately small, dependency-free port of src/codec.rs::gorilla_encode.
// The demo tries Gorilla and raw; the Rust database tries additional codecs.
// compressed contains one persisted v3 block (not an entire database file):
// 54-byte header + codec payload + 4-byte payload checksum. Series/file framing
// is excluded. raw contains little-endian i64 timestamp + IEEE-754 f64 pairs.
export const BLOCK_OVERHEAD_BYTES = 58;
export const DEMO_CODECS = Object.freeze(['gorilla', 'raw']);

class BitWriter {
  bytes = [];
  current = 0;
  used = 0;

  bit(value) {
    this.current = (this.current << 1) | Number(value);
    if (++this.used === 8) {
      this.bytes.push(this.current);
      this.current = this.used = 0;
    }
  }

  bits(value, width) {
    // Append byte-sized chunks; retaining the low bits also handles signed
    // two's-complement timestamps without converting them through Number.
    while (width > 0) {
      const take = Math.min(8 - this.used, width);
      width -= take;
      const mask = (1n << BigInt(take)) - 1n;
      this.current = (this.current << take) | Number((value >> BigInt(width)) & mask);
      this.used += take;
      if (this.used === 8) {
        this.bytes.push(this.current);
        this.current = this.used = 0;
      }
    }
  }

  finish() {
    if (this.used) this.bytes.push(this.current << (8 - this.used));
    return Buffer.from(this.bytes);
  }
}

function gorilla(raw, count) {
  const writer = new BitWriter();
  let previousTs = raw.readBigInt64LE(0);
  let previousBits = raw.readBigUInt64LE(8);
  let previousDelta = 0n;
  let window;
  writer.bits(previousTs, 64);
  writer.bits(previousBits, 64);

  for (let i = 1; i < count; i++) {
    const ts = raw.readBigInt64LE(i * 16);
    const bits = raw.readBigUInt64LE(i * 16 + 8);
    const delta = BigInt.asIntN(64, ts - previousTs);
    if (i === 1) {
      writer.bits(delta, 64);
    } else {
      const dod = BigInt.asIntN(64, delta - previousDelta);
      if (dod === 0n) writer.bit(0);
      else if (dod >= -64n && dod <= 63n) {
        writer.bits(0b10n, 2);
        writer.bits(dod, 7);
      } else if (dod >= -256n && dod <= 255n) {
        writer.bits(0b110n, 3);
        writer.bits(dod, 9);
      } else if (dod >= -2048n && dod <= 2047n) {
        writer.bits(0b1110n, 4);
        writer.bits(dod, 12);
      } else {
        writer.bits(0b1111n, 4);
        writer.bits(dod, 64);
      }
    }
    previousTs = ts;
    previousDelta = delta;

    const xor = previousBits ^ bits;
    previousBits = bits;
    if (xor === 0n) {
      writer.bit(0);
      continue;
    }
    writer.bit(1);
    const binary = xor.toString(2);
    const leading = Math.min(31, 64 - binary.length);
    const trailing = binary.length - 1 - binary.lastIndexOf('1');
    const width = 64 - leading - trailing;
    if (window && leading >= window.leading && trailing >= window.trailing
        && 64 - window.leading - window.trailing <= 11 + width) {
      writer.bit(0);
      writer.bits(xor >> BigInt(window.trailing), 64 - window.leading - window.trailing);
    } else {
      writer.bit(1);
      writer.bits(BigInt(leading), 5);
      writer.bits(BigInt(width - 1), 6);
      writer.bits(xor >> BigInt(trailing), width);
      window = { leading, trailing };
    }
  }
  return writer.finish();
}

const crcTable = Uint32Array.from({ length: 256 }, (_, index) => {
  let crc = index;
  for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
  return crc >>> 0;
});

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) crc = crcTable[(crc ^ byte) & 255] ^ (crc >>> 8);
  return (~crc) >>> 0;
}

/** Encode 1..1,048,576 finite readings with safe integer millisecond timestamps.
 * Every accepted timestamp and every float bit (including -0) is preserved.
 * Equal payload sizes select raw, as in Rust Block::build.
 * @param {Array<{ts: number, value: number}>} points
 * @returns {{raw: Buffer, compressed: Buffer, codec: 'gorilla'|'raw'}}
 */
export function encodeBatch(points) {
  if (!Array.isArray(points) || points.length < 1 || points.length > 1_048_576) {
    throw new RangeError('A batch must contain 1..1,048,576 readings');
  }
  const raw = Buffer.allocUnsafe(points.length * 16);
  let tMin = Infinity;
  let tMax = -Infinity;
  let vMin = Infinity;
  let vMax = -Infinity;
  let sum = 0;
  for (const [i, point] of points.entries()) {
    if (!point || !Number.isSafeInteger(point.ts) || !Number.isFinite(point.value)) {
      throw new TypeError('Readings require safe integer timestamps and finite numeric values');
    }
    raw.writeBigInt64LE(BigInt(point.ts), i * 16);
    raw.writeDoubleLE(point.value, i * 16 + 8);
    tMin = Math.min(tMin, point.ts);
    tMax = Math.max(tMax, point.ts);
    vMin = Math.min(vMin, point.value);
    vMax = Math.max(vMax, point.value);
    sum += point.value;
  }

  const candidate = gorilla(raw, points.length);
  const codec = candidate.length < raw.length ? 'gorilla' : 'raw';
  const payload = codec === 'gorilla' ? candidate : raw;
  const compressed = Buffer.allocUnsafe(BLOCK_OVERHEAD_BYTES + payload.length);
  compressed[0] = codec === 'gorilla' ? 1 : 0;
  compressed[1] = Number(Number.isFinite(sum));
  compressed.writeUInt32LE(points.length, 2);
  compressed.writeBigInt64LE(BigInt(tMin), 6);
  compressed.writeBigInt64LE(BigInt(tMax), 14);
  compressed.writeDoubleLE(vMin, 22);
  compressed.writeDoubleLE(vMax, 30);
  compressed.writeDoubleLE(Number.isFinite(sum) ? sum : NaN, 38);
  compressed.writeUInt32LE(payload.length, 46);
  compressed.writeUInt32LE(crc32(compressed.subarray(0, 50)), 50);
  payload.copy(compressed, 54);
  compressed.writeUInt32LE(crc32(payload), 54 + payload.length);
  return { raw, compressed, codec };
}
