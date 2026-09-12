import assert from 'node:assert/strict';
import test from 'node:test';
import { encodeNative, MAX_FRAME_BYTES, NativeDecoder } from './native-messaging.mjs';

test('native framing survives every split, UTF-8, newlines and coalesced messages', () => {
  const values = [{ text: 'NAVI\nمرحبا\r\n🌐\u2028\u2029' }, { done: true }];
  for (const order of ['LE', 'BE']) {
    const bytes = Buffer.concat(values.map(value => encodeNative(value, order)));
    for (let split = 0; split <= bytes.length; split++) {
      const received = [];
      const decoder = new NativeDecoder(value => received.push(value), order);
      decoder.push(bytes.subarray(0, split));
      decoder.push(bytes.subarray(split));
      decoder.finish();
      assert.deepEqual(received, values);
    }
    const received = [];
    const decoder = new NativeDecoder(value => received.push(value), order);
    for (const byte of bytes) decoder.push(Buffer.from([byte]));
    decoder.finish();
    assert.deepEqual(received, values);
  }
});

test('framing counts UTF-8 bytes, not JavaScript characters', () => {
  const value = { text: '🌐' };
  const frame = encodeNative(value, 'LE');
  assert.equal(frame.readUInt32LE(), Buffer.byteLength(JSON.stringify(value), 'utf8'));
  assert.notEqual(frame.readUInt32LE(), JSON.stringify(value).length);
});

test('oversized headers are rejected before allocating a body, and cannot resync silently', () => {
  for (const length of [0, MAX_FRAME_BYTES + 1, 0xffffffff]) {
    const header = Buffer.alloc(4);
    header.writeUInt32LE(length);
    const decoder = new NativeDecoder(() => assert.fail('Must not dispatch'), 'LE');
    assert.throws(() => decoder.push(header), /length/);
    assert.equal(decoder.body, null);
    assert.throws(() => decoder.push(encodeNative({ ok: true })), /closed/);
  }
  assert.throws(() => encodeNative({ text: 'x'.repeat(MAX_FRAME_BYTES) }), /limit/);
});

test('exact cap is supported; a partial header or body at EOF is a protocol error', () => {
  const frame = encodeNative({ text: 'x'.repeat(MAX_FRAME_BYTES - 11) }, 'LE');
  assert.equal(frame.readUInt32LE(), MAX_FRAME_BYTES);
  let received = 0;
  const complete = new NativeDecoder(() => received++, 'LE');
  complete.push(frame);
  complete.finish();
  assert.equal(received, 1);
  for (const length of [1, 2, 3, 4, 5, frame.length - 1]) {
    const decoder = new NativeDecoder(() => assert.fail('Incomplete frame'), 'LE');
    decoder.push(frame.subarray(0, length));
    assert.throws(() => decoder.finish(), /Truncated/);
  }
});

test('malformed JSON, invalid UTF-8 and non-object messages fail closed', () => {
  for (const body of [Buffer.from('{'), Buffer.from('null'), Buffer.from('[]'), Buffer.from([0xff])]) {
    const header = Buffer.alloc(4);
    header.writeUInt32LE(body.length);
    const decoder = new NativeDecoder(() => assert.fail('Invalid message'), 'LE');
    assert.throws(() => decoder.push(Buffer.concat([header, body])));
  }
});
