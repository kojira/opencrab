import { Link, useLocation } from 'react-router-dom';

interface NavItem {
  to: string;
  label: string;
  icon: string;
  match: (path: string) => boolean;
}

const navItems: NavItem[] = [
  {
    to: '/',
    label: 'Dashboard',
    icon: 'dashboard',
    match: (p) => p === '/',
  },
  {
    to: '/agents',
    label: 'Agents',
    icon: 'smart_toy',
    match: (p) =>
      p === '/agents' ||
      p.startsWith('/agents/') ||
      p.startsWith('/workspace/'),
  },
  {
    to: '/skills',
    label: 'Skills',
    icon: 'psychology',
    match: (p) => p === '/skills',
  },
  {
    to: '/memory',
    label: 'Memory',
    icon: 'memory',
    match: (p) => p === '/memory',
  },
  {
    to: '/sessions',
    label: 'Sessions',
    icon: 'forum',
    match: (p) => p === '/sessions' || p.startsWith('/sessions/'),
  },
  {
    to: '/analytics',
    label: 'Analytics',
    icon: 'analytics',
    match: (p) => p === '/analytics',
  },
];

export default function Sidebar() {
  const location = useLocation();

  return (
    <nav className="w-72 bg-surface-container flex flex-col border-r border-outline-variant">
      {/* Logo */}
      <div className="px-7 py-6">
        <div className="flex items-center gap-3">
          <img src="/icon.png" alt="OpenCrab" className="w-10 h-10 rounded-lg" />
          <div>
            <h1 className="text-title-lg text-on-surface font-semibold">
              OpenCrab
            </h1>
            <p className="text-label-sm text-on-surface-variant">
              Agent Framework
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
              <span>{item.label}</span>
            </Link>
          );
        })}
      </div>

      {/* Footer */}
      <div className="px-7 py-4 border-t border-outline-variant">
        <p className="text-label-sm text-on-surface-variant">
          OpenCrab v0.1.0
        </p>
      </div>
    </nav>
  );
}
