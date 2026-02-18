interface Props {
  logType: string;
  content: string;
  speakerId: string | null;
  createdAt: string;
}

export default function SessionLogItem({ logType, content, speakerId, createdAt }: Props) {
  const config: Record<string, { border: string; icon: string; color: string }> = {
    speech: { border: 'border-l-primary', icon: 'chat_bubble', color: 'text-primary' },
    inner_voice: { border: 'border-l-purple-500', icon: 'psychology', color: 'text-purple-500' },
    action: { border: 'border-l-tertiary', icon: 'bolt', color: 'text-tertiary' },
    system: { border: 'border-l-secondary', icon: 'settings', color: 'text-secondary' },
  };
  const { border, icon, color } = config[logType] ?? { border: 'border-l-outline', icon: 'help', color: 'text-on-surface-variant' };

  return (
    <div className={`bg-surface-container rounded-lg border-l-4 ${border} p-4`}>
      <div className="flex items-center justify-between mb-2">
        <div className="flex items-center gap-2">
          <span className={`material-symbols-outlined text-lg ${color}`}>{icon}</span>
          <span className="text-label-lg text-on-surface">{speakerId || 'unknown'}</span>
        </div>
        <div className="flex items-center gap-2">
          <span className="badge-neutral text-label-sm">{logType}</span>
          <span className="text-body-sm text-on-surface-variant">{createdAt}</span>
        </div>
      </div>
      <p className="text-body-lg text-on-surface whitespace-pre-wrap pl-8">{content}</p>
    </div>
  );
}
