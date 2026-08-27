import { describe, it, expect } from 'vitest';
import {
  LOG_STICK_THRESHOLD_PX,
  distanceFromBottomPx,
  logScrollBehavior,
  shouldFollowLogTail,
} from './logScroll';

describe('distanceFromBottomPx', () => {
  it('is 0 when the viewport sits on the last pixel', () => {
    expect(distanceFromBottomPx(120, 80, 200)).toBe(0);
  });

  it('is 0 when content does not overflow', () => {
    expect(distanceFromBottomPx(0, 400, 200)).toBe(0);
  });

  it('returns the leftover pixels above the tail', () => {
    expect(distanceFromBottomPx(20, 80, 200)).toBe(100);
  });
});

describe('shouldFollowLogTail', () => {
  it('follows when the reader is on the tail', () => {
    expect(
      shouldFollowLogTail({ forceToBottom: false, distanceFromBottomPx: 0 }),
    ).toBe(true);
  });

  it('follows when the leftover is exactly the stick threshold', () => {
    expect(
      shouldFollowLogTail({
        forceToBottom: false,
        distanceFromBottomPx: LOG_STICK_THRESHOLD_PX,
      }),
    ).toBe(true);
  });

  it('does not follow when the reader is farther than the threshold', () => {
    expect(
      shouldFollowLogTail({
        forceToBottom: false,
        distanceFromBottomPx: LOG_STICK_THRESHOLD_PX + 1,
      }),
    ).toBe(false);
  });

  it('always follows after the reader sends, even far from the tail', () => {
    expect(
      shouldFollowLogTail({
        forceToBottom: true,
        distanceFromBottomPx: 10_000,
      }),
    ).toBe(true);
  });

  it('uses the caller threshold when given', () => {
    expect(
      shouldFollowLogTail({
        forceToBottom: false,
        distanceFromBottomPx: 40,
        thresholdPx: 32,
      }),
    ).toBe(false);
  });
});

describe('logScrollBehavior', () => {
  it('jumps when reduced motion is requested', () => {
    expect(logScrollBehavior(true)).toBe('instant');
  });

  it('smooth-scrolls when reduced motion is not requested', () => {
    expect(logScrollBehavior(false)).toBe('smooth');
  });
});
