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
    to: '/skills',
    labelKey: 'nav.skills',
    icon: 'psychology',
    match: (p) => p === '/skills',
  },
  {
    to: '/memory',
    labelKey: 'nav.memory',
    icon: 'memory',
    match: (p) => p === '/memory',
  },
  {
    to: '/sessions',
    labelKey: 'nav.sessions',
    icon: 'forum',
    match: (p) => p === '/sessions' || p.startsWith('/sessions/'),
  },
  {
    to: '/analytics',
    labelKey: 'nav.analytics',
    icon: 'analytics',
    match: (p) => p === '/analytics',
  },
];

export default function Sidebar() {
  const location = useLocation();
  const { t } = useTranslation();

  return (
    <nav className="w-72 bg-surface-container flex flex-col border-r border-outline-variant">
      {/* Logo */}
      <div className="px-7 py-6">
        <div className="flex items-center gap-3">
          <img src="/icon.png" alt="OpenCrab" className="w-10 h-10 rounded-lg" />
          <div>
            <h1 className="text-title-lg text-on-surface font-semibold">
              {t('brand.name')}
            </h1>
            <p className="text-label-sm text-on-surface-variant">
              {t('brand.subtitle')}
            </p>
          </div>
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
