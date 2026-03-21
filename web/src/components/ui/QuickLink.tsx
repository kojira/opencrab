import { Link } from 'react-router-dom';

interface Props {
  to: string;
  icon: string;
  title: string;
}

export function QuickLink({ to, icon, title }: Props) {
  return (
    <Link to={to} className="card-elevated h-20 flex flex-col items-center justify-center gap-2 group">
      <div className="w-10 h-10 rounded-lg bg-primary-container flex items-center justify-center shrink-0 group-hover:bg-primary group-hover:text-primary-on transition-colors">
        <span className="material-symbols-outlined text-xl text-primary group-hover:text-primary-on transition-colors">{icon}</span>
      </div>
      <h3 className="text-title-md text-on-surface group-hover:text-primary transition-colors">{title}</h3>
    </Link>
  );
}
