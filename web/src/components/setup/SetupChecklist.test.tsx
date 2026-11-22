import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { setupStatus } from '../../test/setupStatus';

const getSetupStatus = vi.fn();
vi.mock('../../api/setup', async () => {
  const actual = await vi.importActual<typeof import('../../api/setup')>('../../api/setup');
  return { ...actual, getSetupStatus: () => getSetupStatus() };
});

import SetupChecklist from './SetupChecklist';

beforeEach(() => {
  getSetupStatus.mockReset();
});

describe('SetupChecklist', () => {
  it('lists the four setup steps with their progress', async () => {
    getSetupStatus.mockResolvedValue(setupStatus());
    render(
      <MemoryRouter>
        <SetupChecklist />
      </MemoryRouter>,
    );
    await waitFor(() => expect(screen.getByText('setup.step.gateway')).toBeInTheDocument());
    for (const k of ['llm_provider', 'agent', 'gateway', 'conversation']) {
      expect(screen.getByText(`setup.step.${k}`)).toBeInTheDocument();
    }
    expect(screen.getByText('setup.checklist.progress')).toBeInTheDocument();
  });

  it('shows the done state when all four steps are done', async () => {
    getSetupStatus.mockResolvedValue(setupStatus({ gateway: true, conversation: true }));
    render(
      <MemoryRouter>
        <SetupChecklist />
      </MemoryRouter>,
    );
    await waitFor(() => expect(screen.getByText('setup.checklist.allDone')).toBeInTheDocument());
  });
});
