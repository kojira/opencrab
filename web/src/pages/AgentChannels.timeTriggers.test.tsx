import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor, fireEvent, within } from '@testing-library/react';

vi.mock('../api/schedules', () => ({
  listSchedules: vi.fn(),
  createSchedule: vi.fn(),
  updateSchedule: vi.fn(),
  deleteSchedule: vi.fn(),
}));
vi.mock('../api/sessions', () => ({
  getSessions: vi.fn(),
}));

import { listSchedules, createSchedule, updateSchedule, deleteSchedule } from '../api/schedules';
import { getSessions } from '../api/sessions';
import { TimeTriggersSection } from './AgentChannels';
import type { ScheduleDto, SessionDto } from '../api/types';

const mockedList = vi.mocked(listSchedules);
const mockedCreate = vi.mocked(createSchedule);
const mockedUpdate = vi.mocked(updateSchedule);
const mockedDelete = vi.mocked(deleteSchedule);
const mockedGetSessions = vi.mocked(getSessions);

function session(id: string, theme: string, agentIds: string[]): SessionDto {
  return {
    id,
    mode: 'discord',
    theme,
    phase: 'main',
    turn_number: 0,
    status: 'active',
    participant_count: agentIds.length,
    agent_ids: agentIds,
    metadata_json: null,
  };
}

function schedule(overrides: Partial<ScheduleDto> = {}): ScheduleDto {
  return {
    id: 10,
    agent_id: 'a1',
    session_id: 's1',
    cron_expr: '@every 1800s',
    timezone: 'Asia/Tokyo',
    message: 'patrol',
    enabled: true,
    anchor_at: '2026-01-01T00:00:00+00:00',
    last_fired_at: null,
    next_fire_at: '2026-01-01T00:30:00+00:00',
    gated: false,
    gated_reason: null,
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  mockedGetSessions.mockResolvedValue([
    session('s1', 'General', ['a1']),
    session('s2', 'Other agent', ['a2']),
  ]);
});

describe('TimeTriggersSection', () => {
  it('reads every sessions page so older sessions of this agent can be chosen', async () => {
    const full = Array.from({ length: 100 }, (_, i) => session(`n${i}`, `New ${i}`, ['a2']));
    mockedGetSessions.mockReset();
    mockedGetSessions
      .mockResolvedValueOnce(full)
      .mockResolvedValueOnce([session('old', 'Old channel', ['a1'])]);
    mockedList.mockResolvedValue({ agent_id: 'a1', schedules: [], count: 0 });

    render(<TimeTriggersSection agentId="a1" />);
    await screen.findByText('channels.triggers.empty');

    expect(mockedGetSessions).toHaveBeenNthCalledWith(2, { before: 'n99' });
    fireEvent.click(screen.getByRole('button', { name: /channels.triggers.add/ }));
    const options = within(screen.getByLabelText('channels.triggers.session'))
      .getAllByRole('option')
      .map((o) => o.textContent);
    expect(options).toEqual(['channels.triggers.selectSession', 'Old channel']);
  });

  it('adds an interval trigger as @every Ns and offers only sessions of this agent', async () => {
    mockedList.mockResolvedValueOnce({ agent_id: 'a1', schedules: [], count: 0 });
    mockedList.mockResolvedValueOnce({ agent_id: 'a1', schedules: [schedule()], count: 1 });
    mockedCreate.mockResolvedValue(schedule());

    render(<TimeTriggersSection agentId="a1" />);
    await screen.findByText('channels.triggers.empty');

    fireEvent.click(screen.getByRole('button', { name: /channels.triggers.add/ }));
    const sessionSelect = screen.getByLabelText('channels.triggers.session');
    const options = within(sessionSelect).getAllByRole('option').map((o) => o.textContent);
    expect(options).toEqual(['channels.triggers.selectSession', 'General']);

    fireEvent.change(sessionSelect, { target: { value: 's1' } });
    fireEvent.change(screen.getByLabelText('channels.triggers.intervalSecs'), {
      target: { value: '1800' },
    });
    fireEvent.change(screen.getByLabelText('channels.triggers.prompt'), {
      target: { value: 'patrol' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));

    await waitFor(() => expect(mockedCreate).toHaveBeenCalled());
    expect(mockedCreate).toHaveBeenCalledWith('a1', {
      session_id: 's1',
      cron_expr: '@every 1800s',
      message: 'patrol',
      enabled: true,
    });
    // 再読込後、一覧に次回時刻付きで出る。
    expect(await screen.findByText('@every 1800s')).toBeInTheDocument();
    expect(screen.getByText(new Date('2026-01-01T00:30:00+00:00').toLocaleString())).toBeInTheDocument();
  });

  it('adds a scheduled trigger with cron and timezone', async () => {
    mockedList.mockResolvedValue({ agent_id: 'a1', schedules: [], count: 0 });
    mockedCreate.mockResolvedValue(schedule({ cron_expr: '0 7 * * *' }));

    render(<TimeTriggersSection agentId="a1" />);
    await screen.findByText('channels.triggers.empty');

    fireEvent.click(screen.getByRole('button', { name: /channels.triggers.add/ }));
    fireEvent.change(screen.getByLabelText('channels.triggers.session'), { target: { value: 's1' } });
    fireEvent.change(screen.getByLabelText('channels.triggers.kind'), { target: { value: 'fixed' } });
    fireEvent.change(screen.getByLabelText('channels.triggers.cronExpr'), {
      target: { value: '0 7 * * *' },
    });
    fireEvent.change(screen.getByLabelText('channels.triggers.timezone'), {
      target: { value: 'UTC' },
    });
    fireEvent.change(screen.getByLabelText('channels.triggers.prompt'), {
      target: { value: 'morning summary' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));

    await waitFor(() => expect(mockedCreate).toHaveBeenCalled());
    expect(mockedCreate).toHaveBeenCalledWith('a1', {
      session_id: 's1',
      cron_expr: '0 7 * * *',
      timezone: 'UTC',
      message: 'morning summary',
      enabled: true,
    });
  });

  it('shows a 400 rejection on screen and keeps the form open', async () => {
    mockedList.mockResolvedValue({ agent_id: 'a1', schedules: [], count: 0 });
    mockedCreate.mockRejectedValue(new Error('400 Bad Request'));

    render(<TimeTriggersSection agentId="a1" />);
    await screen.findByText('channels.triggers.empty');

    fireEvent.click(screen.getByRole('button', { name: /channels.triggers.add/ }));
    fireEvent.change(screen.getByLabelText('channels.triggers.session'), { target: { value: 's1' } });
    fireEvent.change(screen.getByLabelText('channels.triggers.intervalSecs'), {
      target: { value: '60' },
    });
    fireEvent.change(screen.getByLabelText('channels.triggers.prompt'), {
      target: { value: 'x' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));

    expect(await screen.findByRole('alert')).toHaveTextContent('channels.triggers.rejected');
    expect(screen.getByLabelText('channels.triggers.prompt')).toBeInTheDocument();
  });

  it('toggles enabled with a PATCH carrying only enabled', async () => {
    mockedList.mockResolvedValue({ agent_id: 'a1', schedules: [schedule()], count: 1 });
    mockedUpdate.mockResolvedValue(schedule({ enabled: false }));

    render(<TimeTriggersSection agentId="a1" />);
    const toggle = await screen.findByRole('checkbox', { name: 'channels.triggers.enabled' });
    fireEvent.click(toggle);

    await waitFor(() => expect(mockedUpdate).toHaveBeenCalledWith(10, { enabled: false }));
  });

  it('edits only the prompt without resending the timing expression', async () => {
    mockedList.mockResolvedValue({
      agent_id: 'a1',
      schedules: [schedule({ cron_expr: '@every 3h' })],
      count: 1,
    });
    mockedUpdate.mockResolvedValue(schedule());

    render(<TimeTriggersSection agentId="a1" />);
    fireEvent.click(await screen.findByRole('button', { name: 'common.edit' }));
    expect(screen.getByLabelText('channels.triggers.intervalSecs')).toHaveValue(10800);
    fireEvent.change(screen.getByLabelText('channels.triggers.prompt'), {
      target: { value: 'new prompt' },
    });
    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));

    await waitFor(() => expect(mockedUpdate).toHaveBeenCalledWith(10, { message: 'new prompt' }));
  });

  it('edits the interval and sends the new @every expression', async () => {
    mockedList.mockResolvedValue({ agent_id: 'a1', schedules: [schedule()], count: 1 });
    mockedUpdate.mockResolvedValue(schedule());

    render(<TimeTriggersSection agentId="a1" />);
    fireEvent.click(await screen.findByRole('button', { name: 'common.edit' }));

    const secs = screen.getByLabelText('channels.triggers.intervalSecs');
    expect(secs).toHaveValue(1800);
    fireEvent.change(secs, { target: { value: '3600' } });
    fireEvent.click(screen.getByRole('button', { name: 'common.save' }));

    await waitFor(() =>
      expect(mockedUpdate).toHaveBeenCalledWith(10, { cron_expr: '@every 3600s' }),
    );
  });

  it('deletes only after confirming the dialog', async () => {
    mockedList.mockResolvedValue({ agent_id: 'a1', schedules: [schedule()], count: 1 });
    mockedDelete.mockResolvedValue({ id: 10, message: 'schedule deleted' });

    render(<TimeTriggersSection agentId="a1" />);
    fireEvent.click(await screen.findByRole('button', { name: 'common.delete' }));
    expect(mockedDelete).not.toHaveBeenCalled();
    expect(screen.getByText('channels.triggers.deleteConfirm')).toBeInTheDocument();

    const dialogButtons = screen.getAllByRole('button', { name: /common.delete/ });
    fireEvent.click(dialogButtons[dialogButtons.length - 1]);

    await waitFor(() => expect(mockedDelete).toHaveBeenCalledWith(10));
  });
});
