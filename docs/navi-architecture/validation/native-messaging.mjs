// Reference framing contract, not a native-host implementation.
import { endianness } from 'node:os';

export const MAX_FRAME_BYTES = 256 * 1024;

function assertObject(value) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('Native messages must be JSON objects');
  }
}

export function encodeNative(value, byteOrder = endianness()) {
  assertObject(value);
  const body = Buffer.from(JSON.stringify(value), 'utf8');
  if (body.length > MAX_FRAME_BYTES) throw new Error('Frame exceeds NAVI limit');
  const header = Buffer.alloc(4);
  header[byteOrder === 'LE' ? 'writeUInt32LE' : 'writeUInt32BE'](body.length);
  return Buffer.concat([header, body]);
}

export class NativeDecoder {
  constructor(onMessage, byteOrder = endianness()) {
    this.onMessage = onMessage;
    this.byteOrder = byteOrder;
    this.header = Buffer.alloc(4);
    this.headerBytes = 0;
    this.body = null;
    this.bodyBytes = 0;
    this.closed = false;
  }

  push(chunk) {
    if (this.closed) throw new Error('Decoder is closed');
    if (!Buffer.isBuffer(chunk)) throw new TypeError('Expected a Buffer');
    let offset = 0;
    try {
      while (offset < chunk.length) {
        if (this.body === null) {
          const count = Math.min(4 - this.headerBytes, chunk.length - offset);
          chunk.copy(this.header, this.headerBytes, offset, offset + count);
          offset += count;
          this.headerBytes += count;
          if (this.headerBytes < 4) continue;
          const length = this.header[this.byteOrder === 'LE' ? 'readUInt32LE' : 'readUInt32BE']();
          // Check before allocation. Never buffer a sender-declared unlimited body.
          if (length === 0 || length > MAX_FRAME_BYTES) throw new Error('Invalid frame length');
          this.body = Buffer.allocUnsafe(length);
          this.bodyBytes = 0;
        }
        const count = Math.min(this.body.length - this.bodyBytes, chunk.length - offset);
        chunk.copy(this.body, this.bodyBytes, offset, offset + count);
        offset += count;
        this.bodyBytes += count;
        if (this.bodyBytes !== this.body.length) continue;
        const value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(this.body));
        assertObject(value);
        this.body = null;
        this.headerBytes = 0;
        this.bodyBytes = 0;
        // Production transport must supply a bounded consumer, not an unlimited queue.
        this.onMessage(value);
      }
    } catch (error) {
      this.closed = true;
      this.body = null;
      throw error;
    }
  }

  finish() {
    if (this.closed) throw new Error('Decoder is closed');
    this.closed = true;
    const incomplete = this.headerBytes !== 0 || this.body !== null;
    this.body = null;
    if (incomplete) throw new Error('Truncated native message');
  }
}
