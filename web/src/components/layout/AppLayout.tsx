import { Outlet } from 'react-router-dom';
import Sidebar from './Sidebar';
import Header from './Header';

export function AppLayout() {
  return (
    <div className="flex h-screen bg-surface font-sans">
      <Sidebar />
      <div className="flex-1 flex flex-col overflow-hidden">
        <Header />
        <main className="flex-1 overflow-y-auto bg-surface p-6">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
