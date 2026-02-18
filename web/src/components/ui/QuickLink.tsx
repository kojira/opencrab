import { Link } from 'react-router-dom';

interface Props {
  to: string;
  icon: string;
  title: string;
  description: string;
}

export function QuickLink({ to, icon, title, description }: Props) {
  return (
    <Link to={to} className="card-elevated flex items-start gap-4 group">
      <div className="w-10 h-10 rounded-lg bg-primary-container flex items-center justify-center shrink-0 group-hover:bg-primary group-hover:text-primary-on transition-colors">
        <span className="material-symbols-outlined text-xl text-primary group-hover:text-primary-on transition-colors">{icon}</span>
      </div>
      <div>
        <h3 className="text-title-md text-on-surface group-hover:text-primary transition-colors mb-1">{title}</h3>
        <p className="text-body-md text-on-surface-variant">{description}</p>
      </div>
    </Link>
  );
}
