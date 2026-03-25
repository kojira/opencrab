import { api } from './client';

export interface LogLevelResponse {
  log_level: string;
}

export function getLogLevel(): Promise<LogLevelResponse> {
  return api.get<LogLevelResponse>('/system/log-level');
}

export function patchLogLevel(level: string): Promise<LogLevelResponse> {
  return api.patch<LogLevelResponse>('/system/log-level', { log_level: level });
}
