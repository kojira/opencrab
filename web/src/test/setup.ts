import '@testing-library/jest-dom';
import { vi } from 'vitest';

Object.defineProperty(window, 'matchMedia', {
  writable: true,
  configurable: true,
  value: (query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  }),
});

Element.prototype.scrollTo = function scrollTo(arg?: ScrollToOptions | number, y?: number) {
  if (typeof arg === 'number') {
    this.scrollLeft = arg;
    this.scrollTop = y ?? 0;
    return;
  }
  if (arg && typeof arg === 'object') {
    if (arg.left != null) this.scrollLeft = arg.left;
    if (arg.top != null) this.scrollTop = arg.top;
  }
};

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
