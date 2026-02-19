import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import type { SessionDto } from '../../api/types';

interface DiscordMetadata {
  source: string;
  is_dm: boolean;
  guild_id?: string;
  guild_name?: string;
  guild_icon_url?: string;
  channel_id?: string;
  channel_name?: string;
  dm_user_name?: string;
  dm_user_id?: string;
  dm_user_avatar_url?: string;
}

function parseDiscordMetadata(
  metadataJson: string | null,
): DiscordMetadata | null {
  if (!metadataJson) return null;
  try {
    const parsed = JSON.parse(metadataJson);
    if (parsed.source === 'discord') return parsed as DiscordMetadata;
  } catch {
    // ignore
  }
  return null;
}

function DiscordSessionInfo({ meta }: { meta: DiscordMetadata }) {
  const { t } = useTranslation();

  if (meta.is_dm) {
    return (
      <div className="flex items-center gap-2">
        {meta.dm_user_avatar_url && (
          <img
            src={meta.dm_user_avatar_url}
            alt={meta.dm_user_name || ''}
            className="w-5 h-5 rounded-full"
          />
        )}
        <span className="badge-neutral text-label-sm">{t('sessionCard.dm')}</span>
      </div>
    );
  }

  return (
    <div className="flex items-center gap-2 flex-wrap">
      {meta.guild_icon_url && (
        <img
          src={meta.guild_icon_url}
          alt={meta.guild_name || ''}
          className="w-5 h-5 rounded-full"
        />
      )}
      {meta.guild_name && (
        <span className="text-body-sm text-on-surface-variant">{meta.guild_name}</span>
      )}
      {meta.channel_name && (
        <span className="text-body-sm text-on-surface-variant">#{meta.channel_name}</span>
      )}
    </div>
  );
}

export default function SessionCard({ session }: { session: SessionDto }) {
  const { t } = useTranslation();

  const badgeClass =
    session.status === 'active'
      ? 'badge-success'
      : session.status === 'completed'
        ? 'badge-info'
        : session.status === 'paused'
          ? 'badge-warning'
          : 'badge-neutral';

  const statusIcon =
    session.status === 'active'
      ? 'play_circle'
      : session.status === 'completed'
        ? 'check_circle'
        : session.status === 'paused'
          ? 'pause_circle'
          : 'help';

  const discordMeta =
    session.mode === 'discord'
      ? parseDiscordMetadata(session.metadata_json)
      : null;

  const sessionIcon = discordMeta ? (
    discordMeta.is_dm && discordMeta.dm_user_avatar_url ? (
      <img
        src={discordMeta.dm_user_avatar_url}
        alt={discordMeta.dm_user_name || ''}
        className="w-10 h-10 rounded-full"
      />
    ) : discordMeta.guild_icon_url ? (
      <img
        src={discordMeta.guild_icon_url}
        alt={discordMeta.guild_name || ''}
        className="w-10 h-10 rounded-full"
      />
    ) : (
      <div className="w-10 h-10 rounded-lg bg-tertiary-container flex items-center justify-center">
        <span className="material-symbols-outlined text-xl text-tertiary">forum</span>
      </div>
    )
  ) : (
    <div className="w-10 h-10 rounded-lg bg-tertiary-container flex items-center justify-center">
      <span className="material-symbols-outlined text-xl text-tertiary">forum</span>
    </div>
  );

  return (
    <Link to={`/sessions/${session.id}`} className="card-elevated block group">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-4 flex-1 min-w-0">
          <div className="shrink-0">{sessionIcon}</div>
          <div className="min-w-0">
            <h3 className="text-title-md text-on-surface group-hover:text-primary transition-colors truncate">
              {session.theme}
            </h3>
            <div className="flex items-center gap-3 text-body-sm text-on-surface-variant mt-0.5">
              <span className="flex items-center gap-1">
                <span className="material-symbols-outlined text-sm">settings</span>
                {session.mode}
              </span>
              <span className="flex items-center gap-1">
                <span className="material-symbols-outlined text-sm">flag</span>
                {session.phase}
              </span>
              <span className="flex items-center gap-1">
                <span className="material-symbols-outlined text-sm">replay</span>
                {t('sessionCard.turn', { number: session.turn_number })}
              </span>
            </div>
            {discordMeta && (
              <div className="mt-1">
                <DiscordSessionInfo meta={discordMeta} />
              </div>
            )}
          </div>
        </div>
        <div className="flex items-center gap-3 shrink-0">
          <span className="chip text-body-sm">
            <span className="material-symbols-outlined text-sm">group</span>
            {session.participant_count}
          </span>
          <span className={badgeClass}>
            <span className="material-symbols-outlined text-sm mr-0.5">{statusIcon}</span>
            {session.status}
          </span>
        </div>
      </div>
    </Link>
  );
}
