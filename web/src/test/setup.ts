import '@testing-library/jest-dom';
import { vi } from 'vitest';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: (key: string, opts?: Record<string, unknown>) => {
      if (opts) {
        // 実物の i18next と同様、キーが未解決なら defaultValue を返す
        // （AgentCard/SessionCard のステータスバッジが依存している）。
        if (typeof opts.defaultValue === 'string') {
          return opts.defaultValue;
        }
        return Object.entries(opts).reduce(
          (s, [k, v]) => s.replace(`{{${k}}}`, String(v)),
          key,
        );
      }
      return key;
    },
    i18n: {
      changeLanguage: vi.fn(),
      language: 'en',
    },
  }),
}));
