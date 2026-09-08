import { describe, it, expect, afterEach } from 'vitest';
import { uuidV4 } from './uuid';

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const originalRandomUUID = crypto.randomUUID;

afterEach(() => {
  Object.defineProperty(crypto, 'randomUUID', {
    configurable: true,
    writable: true,
    value: originalRandomUUID,
  });
});

describe('uuidV4', () => {
  it('returns canonical lowercase UUIDv4 without calling randomUUID', () => {
    const original = crypto.randomUUID;
    let called = false;
    Object.defineProperty(crypto, 'randomUUID', {
      configurable: true,
      writable: true,
      value: () => {
        called = true;
        return original.call(crypto);
      },
    });
    const id = uuidV4();
    expect(id).toMatch(UUID_V4);
    expect(id).toBe(id.toLowerCase());
    expect(called).toBe(false);
  });

  it('still generates when randomUUID is undefined (non-secure context)', () => {
    Object.defineProperty(crypto, 'randomUUID', {
      configurable: true,
      writable: true,
      value: undefined,
    });
    expect(crypto.randomUUID).toBeUndefined();
    const a = uuidV4();
    const b = uuidV4();
    expect(a).toMatch(UUID_V4);
    expect(b).toMatch(UUID_V4);
    expect(a).not.toBe(b);
  });
});
