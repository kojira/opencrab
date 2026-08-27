/** DESIGN-WEBGATE §7.2c: 下端からこの距離以内なら新着に追従する。 */
export const LOG_STICK_THRESHOLD_PX = 80;

/** DESIGN-WEBGATE §7.2c-r1: scrollend 非発火でも追従判定を戻す上限。 */
export const LOG_SCROLLEND_TIMEOUT_MS = 1000;

export type LogScrollBehavior = 'smooth' | 'instant';

export function distanceFromBottomPx(
  scrollTop: number,
  clientHeight: number,
  scrollHeight: number,
): number {
  return Math.max(0, scrollHeight - clientHeight - scrollTop);
}

export function shouldFollowLogTail(input: {
  forceToBottom: boolean;
  distanceFromBottomPx: number;
}): boolean {
  if (input.forceToBottom) return true;
  return input.distanceFromBottomPx <= LOG_STICK_THRESHOLD_PX;
}

export function logScrollBehavior(prefersReducedMotion: boolean): LogScrollBehavior {
  return prefersReducedMotion ? 'instant' : 'smooth';
}
