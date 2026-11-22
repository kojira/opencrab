import { api } from './client';

export interface LlmModelChoices {
  default_model: string;
  choices: string[];
  /** Providers whose models could not be listed (not in `choices`). */
  unavailable_providers: { provider: string; error: string }[];
}

export function getLlmModelChoices(): Promise<LlmModelChoices> {
  return api.get<LlmModelChoices>('/llm/model-choices');
}
