interface Props {
  label: string;
  value: number;
  onChange: (v: number) => void;
}

export default function PersonalitySlider({ label, value, onChange }: Props) {
  const pct = Math.round(value * 100);

  return (
    <div>
      <div className="flex justify-between mb-2">
        <span className="text-label-lg text-on-surface">{label}</span>
        <span className="text-label-md text-primary font-mono">{value.toFixed(2)}</span>
      </div>
      <div className="relative">
        <input
          type="range"
          className="m3-slider"
          min="0"
          max="1"
          step="0.05"
          value={value}
          onChange={(e) => onChange(parseFloat(e.target.value))}
        />
        <div
          className="absolute top-1/2 left-0 h-1 bg-primary rounded-full pointer-events-none -translate-y-1/2"
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}
