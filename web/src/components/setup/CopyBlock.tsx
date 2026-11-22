import { useState } from 'react';
import { useTranslation } from 'react-i18next';

/** A command (or value) shown as-is with a copy button. */
export default function CopyBlock({ text, label }: { text: string; label?: string }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      setCopied(false);
    }
  };

  return (
    <div className="space-y-1">
      {label && <p className="text-label-lg text-on-surface">{label}</p>}
      <div className="relative rounded-md bg-surface-container">
        <pre className="p-3 pr-20 text-xs text-on-surface whitespace-pre-wrap break-all font-mono">
          {text}
        </pre>
        <button
          type="button"
          className="btn-text text-xs absolute top-1.5 right-1.5"
          onClick={() => void copy()}
        >
          <span className="material-symbols-outlined text-base">
            {copied ? 'check' : 'content_copy'}
          </span>
          {copied ? t('setup.copied') : t('setup.copy')}
        </button>
      </div>
    </div>
  );
}
