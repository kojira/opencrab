import { useState, useEffect, type FormEvent } from 'react';
import { Link, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getSession, getSessionLogs, sendOwnerInstruction } from '../api/sessions';
import type { SessionDto, SessionLogRow } from '../api/types';

interface LogMetadata {
  source?: string;
  user_name?: string;
  user_avatar_url?: string;
  [key: string]: unknown;
}

function parseLogMetadata(metadataJson: string | null): LogMetadata | null {
  if (!metadataJson) return null;
  try {
    return JSON.parse(metadataJson) as LogMetadata;
  } catch {
    return null;
  }
}

function SessionLogItem({
  logType,
  content,
  speakerId,
  metadataJson,
}: {
  logType: string;
  content: string;
  speakerId: string | null;
  metadataJson: string | null;
}) {
  const [borderColor, icon, iconColor] = (() => {
    switch (logType) {
      case 'speech':
        return ['border-l-primary', 'chat_bubble', 'text-primary'];
      case 'inner_voice':
        return ['border-l-purple-500', 'psychology', 'text-purple-500'];
      case 'action':
        return ['border-l-tertiary', 'bolt', 'text-tertiary'];
      case 'system':
        return ['border-l-secondary', 'settings', 'text-secondary'];
      default:
        return ['border-l-outline', 'help', 'text-on-surface-variant'];
    }
  })();

  const meta = parseLogMetadata(metadataJson);
  const isDiscordUser = meta?.source === 'discord';
  const isDiscordResponse = meta?.source === 'discord_response';

  // Determine speaker display
  let speakerDisplay: React.ReactNode;
  if (isDiscordUser && meta) {
    speakerDisplay = (
      <div className="flex items-center gap-2">
        {meta.user_avatar_url ? (
          <img
            src={meta.user_avatar_url}
            alt={meta.user_name || ''}
            className="w-6 h-6 rounded-full"
          />
        ) : (
          <span className={`material-symbols-outlined text-lg ${iconColor}`}>{icon}</span>
        )}
        <span className="text-label-lg text-on-surface">
          {meta.user_name || speakerId || ''}
        </span>
        {speakerId && meta.user_name && (
          <span className="text-body-sm text-on-surface-variant">({speakerId})</span>
        )}
      </div>
    );
  } else if (isDiscordResponse && speakerId) {
    const initial = speakerId.charAt(0).toUpperCase();
    speakerDisplay = (
      <div className="flex items-center gap-2">
        <div className="w-6 h-6 rounded-full bg-primary-container flex items-center justify-center">
          <span className="text-xs text-primary font-medium">{initial}</span>
        </div>
        <span className="text-label-lg text-on-surface">{speakerId}</span>
      </div>
    );
  } else {
    speakerDisplay = (
      <div className="flex items-center gap-2">
        <span className={`material-symbols-outlined text-lg ${iconColor}`}>{icon}</span>
        <span className="text-label-lg text-on-surface">{speakerId || ''}</span>
      </div>
    );
  }

  return (
    <div
      className={`bg-surface-container rounded-lg border-l-4 ${borderColor} p-4`}
    >
      <div className="flex items-center justify-between mb-2">
        {speakerDisplay}
        <div className="flex items-center gap-2">
          <span className="badge-neutral text-label-sm">{logType}</span>
        </div>
      </div>
      <p className="text-body-lg text-on-surface whitespace-pre-wrap pl-8">
        {content}
      </p>
    </div>
  );
}

export default function SessionDetail() {
  const { t } = useTranslation();
  const { id } = useParams<{ id: string }>();
  const [session, setSession] = useState<SessionDto | null>(null);
  const [logs, setLogs] = useState<SessionLogRow[] | null>(null);
  const [logsError, setLogsError] = useState<string | null>(null);
  const [ownerInput, setOwnerInput] = useState('');

  const loadLogs = () => {
    if (!id) return;
    getSessionLogs(id)
      .then(setLogs)
      .catch((e: Error) => setLogsError(e.message));
  };

  useEffect(() => {
    if (!id) return;
    getSession(id).then(setSession).catch(() => {});
    loadLogs();
  }, [id]); // eslint-disable-line react-hooks/exhaustive-deps

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    if (!id || !ownerInput.trim()) return;
    const content = ownerInput.trim();
    setOwnerInput('');
    await sendOwnerInstruction(id, content);
    loadLogs();
  };

  const badgeClass = session
    ? session.status === 'active'
      ? 'badge-success'
      : session.status === 'completed'
        ? 'badge-info'
        : session.status === 'paused'
          ? 'badge-warning'
          : 'badge-neutral'
    : '';

  const statusIcon = session
    ? session.status === 'active'
      ? 'play_circle'
      : session.status === 'completed'
        ? 'check_circle'
        : session.status === 'paused'
          ? 'pause_circle'
          : 'help'
    : '';

  return (
    <div className="max-w-4xl mx-auto h-full flex flex-col">
      {/* Session header */}
      {session ? (
        <div className="card-elevated mb-4">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-4">
              <Link to="/sessions" className="btn-text p-1">
                <span className="material-symbols-outlined">arrow_back</span>
              </Link>
              <div>
                <h1 className="text-title-lg text-on-surface">
                  {session.theme}
                </h1>
                <div className="flex items-center gap-3 text-body-sm text-on-surface-variant mt-0.5">
                  <span>{t('sessionDetail.mode', { value: session.mode })}</span>
                  <span>{t('sessionDetail.phase', { value: session.phase })}</span>
                  <span>{t('sessionDetail.turn', { value: session.turn_number })}</span>
                </div>
              </div>
            </div>
            <span className={badgeClass}>
              <span className="material-symbols-outlined text-sm mr-0.5">
                {statusIcon}
              </span>
              {session.status}
            </span>
          </div>
        </div>
      ) : (
        <div className="card-elevated mb-4">
          <p className="text-body-lg text-on-surface-variant">
            {t('sessionDetail.loadingSession')}
          </p>
        </div>
      )}

      {/* Log entries */}
      <div className="flex-1 overflow-y-auto space-y-2 mb-4">
        {logsError ? (
          <div className="card-outlined border-error bg-error-container/30 p-4">
            <div className="flex items-center gap-2">
              <span className="material-symbols-outlined text-error">
                error
              </span>
              <p className="text-body-lg text-error-on-container">
                {t('common.error', { message: logsError })}
              </p>
            </div>
          </div>
        ) : logs === null ? (
          <div className="empty-state">
            <p className="text-body-lg text-on-surface-variant">
              {t('sessionDetail.loadingLogs')}
            </p>
          </div>
        ) : logs.length === 0 ? (
          <div className="empty-state">
            <span className="material-symbols-outlined empty-state-icon">
              chat
            </span>
            <p className="empty-state-text">{t('sessionDetail.noLogs')}</p>
          </div>
        ) : (
          logs.map((log) => (
            <SessionLogItem
              key={log.id}
              logType={log.log_type}
              content={log.content}
              speakerId={log.speaker_id}
              metadataJson={log.metadata_json}
            />
          ))
        )}
      </div>

      {/* Owner input */}
      <div className="card-elevated">
        <form className="flex gap-3" onSubmit={handleSubmit}>
          <input
            type="text"
            className="input-outlined flex-1"
            placeholder={t('sessionDetail.ownerPlaceholder')}
            value={ownerInput}
            onChange={(e) => setOwnerInput(e.target.value)}
          />
          <button type="submit" className="btn-filled">
            <span className="material-symbols-outlined text-xl">send</span>
            {t('common.send')}
          </button>
        </form>
      </div>
    </div>
  );
}
