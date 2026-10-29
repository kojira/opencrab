import { describe, it, expect } from 'vitest';
import { agentTabs } from './AgentLayout';

// D-1097: Discord / Nostr settings (co-agents, trusted users, channels) are owned and served by
// the gateways. The agent page keeps only core settings; the channels tab is time triggers only.
describe('AgentLayout tabs', () => {
  it('has no gateway settings tabs', () => {
    const keys = agentTabs.map((tab) => tab.key);
    for (const removed of ['co-agents', 'trusted-users', 'channels']) {
      expect(keys).not.toContain(removed);
    }
  });

  it('keeps time triggers as their own tab', () => {
    const tab = agentTabs.find((t) => t.key === 'time-triggers');
    expect(tab).toEqual(
      expect.objectContaining({ path: '/time-triggers', labelKey: 'agentNav.timeTriggers' }),
    );
  });
});
