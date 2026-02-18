interface Props {
  label: string;
  value: string;
}

export default function DetailRow({ label, value }: Props) {
  return (
    <div className="flex items-center py-2">
      <span className="w-36 text-label-lg text-on-surface-variant">{label}</span>
      <span className="text-body-lg text-on-surface font-mono">{value}</span>
    </div>
  );
}
