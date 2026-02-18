import { Link } from 'react-router-dom';

interface Props {
  to: string;
  icon: string;
  title: string;
  description: string;
}

export default function ActionCard({ to, icon, title, description }: Props) {
  return (
    <Link to={to} className="card-elevated text-center group">
      <span className="material-symbols-outlined text-3xl text-primary mb-2 group-hover:scale-110 transition-transform">{icon}</span>
      <h3 className="text-title-md text-on-surface mb-1">{title}</h3>
      <p className="text-body-sm text-on-surface-variant">{description}</p>
    </Link>
  );
}
