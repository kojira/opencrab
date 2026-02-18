import { useState, useEffect, type FormEvent } from 'react';
import { Link, useParams } from 'react-router-dom';
import { getSession, getSessionLogs, sendMentorInstruction } from '../api/sessions';
import type { SessionDto, SessionLogRow } from '../api/types';

function SessionLogItem({
  logType,
  content,
  speakerId,
}: {
  logType: string;
  content: string;
  speakerId: string | null;
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

  return (
    <div
      className={`bg-surface-container rounded-lg border-l-4 ${borderColor} p-4`}
    >
      <div className="flex items-center justify-between mb-2">
        <div className="flex items-center gap-2">
          <span className={`material-symbols-outlined text-lg ${iconColor}`}>
            {icon}
          </span>
          <span className="text-label-lg text-on-surface">
            {speakerId || ''}
          </span>
        </div>
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
  const { id } = useParams<{ id: string }>();
  const [session, setSession] = useState<SessionDto | null>(null);
  const [logs, setLogs] = useState<SessionLogRow[] | null>(null);
  const [logsError, setLogsError] = useState<string | null>(null);
  const [mentorInput, setMentorInput] = useState('');

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
    if (!id || !mentorInput.trim()) return;
    const content = mentorInput.trim();
    setMentorInput('');
    await sendMentorInstruction(id, content);
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
                  <span>Mode: {session.mode}</span>
                  <span>Phase: {session.phase}</span>
                  <span>Turn: {session.turn_number}</span>
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
            Loading session...
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
                Error: {logsError}
              </p>
            </div>
          </div>
        ) : logs === null ? (
          <div className="empty-state">
            <p className="text-body-lg text-on-surface-variant">
              Loading logs...
            </p>
          </div>
        ) : logs.length === 0 ? (
          <div className="empty-state">
            <span className="material-symbols-outlined empty-state-icon">
              chat
            </span>
            <p className="empty-state-text">No logs yet.</p>
          </div>
        ) : (
          logs.map((log) => (
            <SessionLogItem
              key={log.id}
              logType={log.log_type}
              content={log.content}
              speakerId={log.speaker_id}
            />
          ))
        )}
      </div>

      {/* Mentor input */}
      <div className="card-elevated">
        <form className="flex gap-3" onSubmit={handleSubmit}>
          <input
            type="text"
            className="input-outlined flex-1"
            placeholder="Type mentor instruction..."
            value={mentorInput}
            onChange={(e) => setMentorInput(e.target.value)}
          />
          <button type="submit" className="btn-filled">
            <span className="material-symbols-outlined text-xl">send</span>
            Send
          </button>
        </form>
      </div>
    </div>
  );
}
