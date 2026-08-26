import { useState, type FormEvent } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { createWebConversation, ConversationCreateError } from '../../api/sessions';

export default function NewConversationButton({ agentId }: { agentId: string | null }) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const [name, setName] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState('');

  const disabled = !agentId || submitting;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!agentId || submitting) return;
    setSubmitting(true);
    setError('');
    try {
      const trimmed = name.trim();
      const created = await createWebConversation(
        agentId,
        trimmed === '' ? undefined : trimmed,
      );
      setOpen(false);
      navigate(`/sessions/${created.session_id}`, {
        state: { webCreateState: created.state },
      });
    } catch (err) {
      const code =
        err instanceof ConversationCreateError ? err.code : (err as Error).message;
      setError(code);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <>
      <button
        type="button"
        className="btn-filled"
        disabled={disabled}
        aria-label={t('sessions.newConversation')}
        title={!agentId ? t('sessions.selectAgentFirst') : undefined}
        onClick={() => {
          if (!agentId) return;
          setOpen(true);
        }}
      >
        <span className="material-symbols-outlined text-xl">add</span>
        {t('sessions.newConversation')}
      </button>
      {open ? (
        <div className="scrim" onClick={() => !submitting && setOpen(false)}>
          <div className="dialog" onClick={(e) => e.stopPropagation()} role="dialog">
            <h3 className="text-title-lg text-on-surface mb-4">
              {t('sessions.newConversation')}
            </h3>
            <form onSubmit={(e) => void submit(e)}>
              <label className="block mb-4">
                <span className="text-label-lg text-on-surface-variant">
                  {t('sessions.conversationName')}
                </span>
                <input
                  type="text"
                  className="input-outlined w-full mt-1"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  disabled={submitting}
                  autoFocus
                />
              </label>
              {error ? (
                <p className="text-body-sm text-error mb-3" role="alert">
                  {t('common.error', { message: error })}
                </p>
              ) : null}
              <div className="flex gap-3 justify-end">
                <button
                  type="button"
                  className="btn-outlined"
                  disabled={submitting}
                  onClick={() => setOpen(false)}
                >
                  {t('common.cancel')}
                </button>
                <button type="submit" className="btn-filled" disabled={submitting}>
                  {t('common.create')}
                </button>
              </div>
            </form>
          </div>
        </div>
      ) : null}
    </>
  );
}
