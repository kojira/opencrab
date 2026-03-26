import { api } from './client';

export interface LlmModelChoices {
  default_model: string;
  choices: string[];
}

export function getLlmModelChoices(): Promise<LlmModelChoices> {
  return api.get<LlmModelChoices>('/llm/model-choices');
}
