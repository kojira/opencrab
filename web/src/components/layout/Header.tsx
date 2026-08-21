import { useTranslation } from 'react-i18next';

interface HeaderProps {
  onMenuClick?: () => void;
}

export default function Header({ onMenuClick }: HeaderProps) {
  const { t } = useTranslation();

  return (
    <header className="bg-surface-container border-b border-outline-variant px-4 md:px-6 py-3">
      <div className="flex items-center gap-3">
        {/* Hamburger menu button (mobile only) */}
        <button
          className="md:hidden p-2 rounded-full hover:bg-surface-container-high text-on-surface-variant shrink-0"
          onClick={onMenuClick}
          aria-label="Open menu"
        >
          <span className="material-symbols-outlined text-xl">menu</span>
        </button>


{/* Status indicators & language toggle */}
        <div className="flex items-center gap-2 md:gap-4 shrink-0">
          <div className="hidden sm:flex items-center gap-2 px-3 py-1.5 rounded-full bg-success-container">
            <span className="w-2 h-2 rounded-full bg-success animate-pulse" />
            <span className="text-label-md text-success-on-container">
              {t('header.dbConnected')}
            </span>
          </div>
          {/* Mobile status indicator (dot only) */}
          <div className="sm:hidden w-2 h-2 rounded-full bg-success animate-pulse" />

        </div>
      </div>
    </header>
  );
}
