export default function Header() {
  return (
    <header className="bg-surface-container border-b border-outline-variant px-6 py-3">
      <div className="flex items-center justify-between">
        {/* Search bar */}
        <div className="flex-1 max-w-lg">
          <div className="relative">
            <span className="material-symbols-outlined absolute left-3 top-1/2 -translate-y-1/2 text-on-surface-variant text-xl">
              search
            </span>
            <input
              type="text"
              className="w-full pl-11 pr-4 py-2.5 rounded-full bg-surface-container-high text-on-surface text-body-lg placeholder:text-on-surface-variant border-none focus:ring-2 focus:ring-primary/30 focus:outline-none transition-all duration-200"
              placeholder="Search agents, sessions, memories..."
            />
          </div>
        </div>

        {/* Status indicators */}
        <div className="flex items-center gap-4">
          <div className="flex items-center gap-2 px-3 py-1.5 rounded-full bg-success-container">
            <span className="w-2 h-2 rounded-full bg-success animate-pulse" />
            <span className="text-label-md text-success-on-container">
              DB Connected
            </span>
          </div>
        </div>
      </div>
    </header>
  );
}
