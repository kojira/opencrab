import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('./client', () => ({
  api: {
    get: vi.fn(),
    post: vi.fn(),
    put: vi.fn(),
    del: vi.fn(),
    patch: vi.fn(),
  },
}));

import { api } from './client';
import { listSchedules, createSchedule, updateSchedule, deleteSchedule } from './schedules';

const mockedApi = vi.mocked(api);

beforeEach(() => {
  vi.clearAllMocks();
});

describe('schedules API client', () => {
  it('lists schedules of an agent', async () => {
    mockedApi.get.mockResolvedValue({ agent_id: 'a1', schedules: [], count: 0 });
    await listSchedules('a1');
    expect(mockedApi.get).toHaveBeenCalledWith('/agents/a1/schedules');
  });

  it('creates a schedule with the server request shape', async () => {
    mockedApi.post.mockResolvedValue({ id: 1 });
    const body = {
      session_id: 's1',
      cron_expr: '@every 1800s',
      message: 'check inbox',
      enabled: true,
    };
    await createSchedule('a1', body);
    expect(mockedApi.post).toHaveBeenCalledWith('/agents/a1/schedules', body);
  });

  it('patches a schedule by id', async () => {
    mockedApi.patch.mockResolvedValue({ id: 7 });
    await updateSchedule(7, { enabled: false });
    expect(mockedApi.patch).toHaveBeenCalledWith('/schedules/7', { enabled: false });
  });

  it('deletes a schedule by id', async () => {
    mockedApi.del.mockResolvedValue({ id: 7, message: 'schedule deleted' });
    await deleteSchedule(7);
    expect(mockedApi.del).toHaveBeenCalledWith('/schedules/7');
  });
});
