interface Props {
  icon: string;
  iconBg: string;
  iconColor: string;
  label: string;
  value: string;
}

export function StatCard({ icon, iconBg, iconColor, label, value }: Props) {
  return (
    <div className="card-elevated">
      <div className="flex items-center gap-4">
        <div className={`w-12 h-12 rounded-lg ${iconBg} flex items-center justify-center`}>
          <span className={`material-symbols-outlined text-2xl ${iconColor}`}>{icon}</span>
        </div>
        <div>
          <p className="text-body-md text-on-surface-variant">{label}</p>
          <p className="text-headline-md text-on-surface font-semibold">{value}</p>
        </div>
      </div>
    </div>
  );
}
