import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import NewConversationButton from './NewConversationButton';
import { ConversationCreateError } from '../../api/sessions';

const createWebConversation = vi.fn();

vi.mock('../../api/sessions', async () => {
  const actual = await vi.importActual<typeof import('../../api/sessions')>('../../api/sessions');
  return {
    ...actual,
    createWebConversation: (...args: unknown[]) => createWebConversation(...args),
  };
});

const navigate = vi.fn();
vi.mock('react-router-dom', async () => {
  const actual = await vi.importActual<typeof import('react-router-dom')>('react-router-dom');
  return {
    ...actual,
    useNavigate: () => navigate,
  };
});

beforeEach(() => {
  createWebConversation.mockReset();
  navigate.mockReset();
});

function renderButton(agentId: string | null) {
  return render(
    <MemoryRouter>
      <NewConversationButton agentId={agentId} />
    </MemoryRouter>,
  );
}

describe('NewConversationButton', () => {
  it('disables when no agent is selected', () => {
    renderButton(null);
    expect(screen.getByRole('button', { name: 'sessions.newConversation' })).toBeDisabled();
  });

  it('keeps the name and stays retryable on 409', async () => {
    createWebConversation.mockRejectedValue(
      new ConversationCreateError(409, 'web_instance_unavailable', 'web_instance_unavailable'),
    );
    const user = userEvent.setup();
    renderButton('agent-1');
    await user.click(screen.getByRole('button', { name: 'sessions.newConversation' }));
    const name = screen.getByRole('textbox');
    await user.type(name, 'Lunch');
    await user.click(screen.getByRole('button', { name: 'common.create' }));
    await waitFor(() => {
      expect(screen.getByRole('alert')).toBeInTheDocument();
    });
    expect(name).toHaveValue('Lunch');
    expect(createWebConversation).toHaveBeenCalledTimes(1);
    expect(screen.getByRole('button', { name: 'common.create' })).toBeEnabled();
  });

  it('keeps the name on 400 name error', async () => {
    createWebConversation.mockRejectedValue(
      new ConversationCreateError(400, 'name must not contain newlines', 'name must not contain newlines'),
    );
    const user = userEvent.setup();
    renderButton('agent-1');
    await user.click(screen.getByRole('button', { name: 'sessions.newConversation' }));
    await user.type(screen.getByRole('textbox'), 'too-long-name');
    await user.click(screen.getByRole('button', { name: 'common.create' }));
    await waitFor(() => {
      expect(screen.getByRole('alert')).toBeInTheDocument();
    });
    expect(screen.getByRole('textbox')).toHaveValue('too-long-name');
    expect(createWebConversation).toHaveBeenCalledTimes(1);
  });

  it('disables submit while in flight and does not auto-resend', async () => {
    let resolveCreate: (v: unknown) => void = () => undefined;
    createWebConversation.mockReturnValue(
      new Promise((resolve) => {
        resolveCreate = resolve;
      }),
    );
    const user = userEvent.setup();
    renderButton('agent-1');
    await user.click(screen.getByRole('button', { name: 'sessions.newConversation' }));
    await user.click(screen.getByRole('button', { name: 'common.create' }));
    expect(screen.getByRole('button', { name: 'common.create' })).toBeDisabled();
    expect(createWebConversation).toHaveBeenCalledTimes(1);
    resolveCreate({
      conversation_id: 'c',
      session_id: 'web-agent-1-c',
      binding_id: 'b',
      name: null,
      state: 'ready',
      httpStatus: 201,
    });
    await waitFor(() => {
      expect(navigate).toHaveBeenCalledWith('/sessions/web-agent-1-c', {
        state: { webCreateState: 'ready' },
      });
    });
    expect(createWebConversation).toHaveBeenCalledTimes(1);
  });
});
