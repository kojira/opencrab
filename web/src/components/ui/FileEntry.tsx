import type { WorkspaceEntryDto } from '../../api/types';

interface Props {
  entry: WorkspaceEntryDto;
  onClick: (name: string, isDir: boolean) => void;
}

export default function FileEntry({ entry, onClick }: Props) {
  let icon: string;
  let iconColor: string;
  if (entry.is_dir) {
    icon = 'folder';
    iconColor = 'text-warning';
  } else {
    const ext = entry.name.split('.').pop() ?? '';
    switch (ext) {
      case 'rs': icon = 'code'; iconColor = 'text-primary'; break;
      case 'toml': case 'json': case 'yaml': case 'yml': icon = 'settings'; iconColor = 'text-tertiary'; break;
      case 'md': case 'txt': icon = 'article'; iconColor = 'text-secondary'; break;
      default: icon = 'draft'; iconColor = 'text-on-surface-variant';
    }
  }

  return (
    <button
      className="w-full flex items-center gap-3 px-3 py-2.5 rounded-md hover:bg-secondary-container/40 active:bg-secondary-container/60 text-left transition-colors duration-150"
      onClick={() => onClick(entry.name, entry.is_dir)}
    >
      <span className={`material-symbols-outlined text-xl ${iconColor}`}>{icon}</span>
      <span className="flex-1 text-body-md text-on-surface">{entry.name}</span>
      {!entry.is_dir && (
        <span className="text-label-sm text-on-surface-variant">{entry.size} B</span>
      )}
    </button>
  );
}
