interface Props {
  label: string;
  value: string;
}

export default function DetailRow({ label, value }: Props) {
  return (
    <div className="flex items-start py-2 gap-2">
      <span className="w-36 shrink-0 text-label-lg text-on-surface-variant">{label}</span>
      <span className="min-w-0 text-body-lg text-on-surface font-mono break-all">{value}</span>
    </div>
  );
}
