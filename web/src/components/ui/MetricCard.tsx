interface Props {
  icon: string;
  label: string;
  value: string;
}

export default function MetricCard({ icon, label, value }: Props) {
  return (
    <div className="card-elevated">
      <div className="flex items-center gap-2 mb-2">
        <span className="material-symbols-outlined text-lg text-primary">{icon}</span>
        <p className="text-label-lg text-on-surface-variant">{label}</p>
      </div>
      <p className="text-headline-sm text-on-surface font-semibold">{value}</p>
    </div>
  );
}
