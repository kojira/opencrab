import { useState } from 'react';
import { Link, Outlet, useLocation } from 'react-router-dom';
import Sidebar from './Sidebar';
import Header from './Header';

const bottomNavItems = [
  { path: '/', icon: 'dashboard', label: 'Dashboard' },
  { path: '/agents', icon: 'smart_toy', label: 'Agents' },
  { path: '/sessions', icon: 'forum', label: 'Sessions' },
];

export function AppLayout() {
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const location = useLocation();

  return (
    <div className="flex h-screen bg-surface font-sans">
      <Sidebar open={sidebarOpen} onClose={() => setSidebarOpen(false)} />
      {/* Overlay for mobile */}
      {sidebarOpen && (
        <div
          className="fixed inset-0 bg-black/50 z-20 md:hidden"
          onClick={() => setSidebarOpen(false)}
        />
      )}
      <div className="flex-1 flex flex-col overflow-hidden min-w-0">
        <Header onMenuClick={() => setSidebarOpen(true)} />
        <main className="flex-1 overflow-y-auto bg-surface p-4 md:p-6 pb-16 md:pb-0">
          <Outlet />
        </main>
      </div>

      {/* Bottom navigation for mobile */}
      <nav className="fixed bottom-0 left-0 right-0 z-30 bg-surface-container border-t border-outline-variant md:hidden">
        <div className="flex">
          {bottomNavItems.map((item) => {
            const active =
              item.path === '/'
                ? location.pathname === '/'
                : location.pathname.startsWith(item.path);
            return (
              <Link
                key={item.path}
                to={item.path}
                className={`flex-1 flex flex-col items-center justify-center py-2 gap-0.5 min-h-[56px] ${
                  active ? 'text-primary' : 'text-on-surface-variant'
                }`}
              >
                <span className="material-symbols-outlined">{item.icon}</span>
                <span className="text-label-sm">{item.label}</span>
              </Link>
            );
          })}
        </div>
      </nav>
    </div>
  );
}
