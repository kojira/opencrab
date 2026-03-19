import { Link, useLocation } from 'react-router-dom';
import { useTranslation } from 'react-i18next';

interface NavItem {
  to: string;
  labelKey: string;
  icon: string;
  match: (path: string) => boolean;
}

const navItems: NavItem[] = [
  {
    to: '/',
    labelKey: 'nav.dashboard',
    icon: 'dashboard',
    match: (p) => p === '/',
  },
  {
    to: '/agents',
    labelKey: 'nav.agents',
    icon: 'smart_toy',
    match: (p) =>
      p === '/agents' ||
      p.startsWith('/agents/') ||
      p.startsWith('/workspace/'),
  },
  {
    to: '/sessions',
    labelKey: 'nav.sessions',
    icon: 'forum',
    match: (p) => p === '/sessions' || p.startsWith('/sessions/'),
  },
];

interface SidebarProps {
  open?: boolean;
  onClose?: () => void;
}

export default function Sidebar({ open = false, onClose }: SidebarProps) {
  const location = useLocation();
  const { t } = useTranslation();

  return (
    <nav
      className={`
        w-72 bg-surface-container flex flex-col border-r border-outline-variant
        fixed inset-y-0 left-0 z-30 transition-transform duration-300
        md:relative md:translate-x-0 md:z-auto
        ${open ? 'translate-x-0' : '-translate-x-full'}
      `}
    >
      {/* Logo */}
      <div className="px-7 py-6">
        <div className="flex items-center gap-3">
          <img src="/icon.png" alt="OpenCrab" className="w-10 h-10 rounded-lg" />
          <div className="flex-1 min-w-0">
            <h1 className="text-title-lg text-on-surface font-semibold truncate">
              {t('brand.name')}
            </h1>
            <p className="text-label-sm text-on-surface-variant truncate">
              {t('brand.subtitle')}
            </p>
          </div>
          {/* Close button for mobile */}
          <button
            className="md:hidden p-1 rounded-full hover:bg-surface-container-high text-on-surface-variant"
            onClick={onClose}
            aria-label="Close menu"
          >
            <span className="material-symbols-outlined text-xl">close</span>
          </button>
        </div>
      </div>

      {/* Divider */}
      <div className="mx-4 h-px bg-outline-variant" />

      {/* Navigation */}
      <div className="flex-1 px-3 py-4 space-y-1">
        {navItems.map((item) => {
          const active = item.match(location.pathname);
          return (
            <Link
              key={item.to}
              to={item.to}
              className={active ? 'nav-item-active' : 'nav-item'}
              onClick={onClose}
            >
              <span className="material-symbols-outlined text-xl">
                {item.icon}
              </span>
              <span>{t(item.labelKey)}</span>
            </Link>
          );
        })}
      </div>

      {/* Footer */}
      <div className="px-7 py-4 border-t border-outline-variant">
        <p className="text-label-sm text-on-surface-variant">
          {t('brand.version')}
        </p>
      </div>
    </nav>
  );
}
