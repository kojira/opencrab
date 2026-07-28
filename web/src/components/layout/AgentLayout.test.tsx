import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter, Routes, Route } from 'react-router-dom';
import AgentLayout from './AgentLayout';
import type { AgentDetail } from '../../api/types';

const agent: AgentDetail = {
  id: 'a1',
  name: 'のすたろう',
  job_title: null,
  organization: null,
  image_url: null,
  persona_name: 'crab',
  personality: null,
  instructions: '',
  model: null,
  reasoning_effort: null,
  web_search: null,
  metadata_json: null,
};

vi.mock('../../api/agents', () => ({
  getAgent: vi.fn(() => Promise.resolve(agent)),
  deleteAgent: vi.fn(),
}));

/** setup.ts の t() モックはキーをそのまま返すので、キー文字列で照合できる。 */
const tabLabelKeys = [
  'agentNav.overview',
  'agentNav.skills',
  'agentNav.sleep',
  'agentNav.memory',
  'agentNav.sessions',
  'agentNav.coAgents',
  'agentNav.trustedUsers',
  'agentNav.channels',
  'agentNav.allowedCommands',
  'agentNav.llmLogs',
  'agentNav.analytics',
];

async function renderLayout(initialPath = '/agents/a1') {
  render(
    <MemoryRouter initialEntries={[initialPath]}>
      <Routes>
        <Route path="/agents/:id/*" element={<AgentLayout />} />
      </Routes>
    </MemoryRouter>,
  );
  // getAgent の解決を待つ（解決前は loading 表示でタブが出ない）。
  // 名前はパンくずと見出しの2箇所に出るので findAllByText を使う。
  await screen.findAllByText('のすたろう');
}

describe('AgentLayout のタブ', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('11 個すべてのタブがラベル付きで描画される', async () => {
    await renderLayout();
    for (const key of tabLabelKeys) {
      const label = screen.getByText(key);
      expect(label, `${key} のラベルが無い`).toBeInTheDocument();
      expect(label.closest('a')).toHaveAttribute('href');
    }
  });

  it('ラベルがレスポンシブに非表示化されていない', async () => {
    // 以前は `hidden sm:inline` が付いており、スマホ幅でアイコンだけになっていた。
    // jsdom は Tailwind の CSS を評価しないので、クラス名で直接検査する。
    await renderLayout();
    for (const key of tabLabelKeys) {
      const cls = screen.getByText(key).className;
      expect(cls, `${key} のラベルが hidden で隠されている: "${cls}"`).not.toMatch(
        /(^|\s)hidden(\s|$)/,
      );
    }
  });

  it('編集サブルートではタブバーを出さない', async () => {
    await renderLayout('/agents/a1/edit');
    expect(screen.queryByText('agentNav.overview')).not.toBeInTheDocument();
  });
});
