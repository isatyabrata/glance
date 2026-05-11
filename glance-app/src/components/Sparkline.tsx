interface Props {
  values: number[];
  width?: number;
  height?: number;
}

/** A subtle, hairline sparkline. Empty/all-zero data falls back to a flat line. */
export function Sparkline({ values, width = 120, height = 28 }: Props) {
  const w = width;
  const h = height;
  if (!values || values.length === 0) {
    return (
      <svg className="sparkline" viewBox={`0 0 ${w} ${h}`}>
        <path d={`M0 ${h / 2} L${w} ${h / 2}`} />
      </svg>
    );
  }
  const max = Math.max(...values, 1);
  const min = Math.min(...values, 0);
  const range = Math.max(max - min, 1);
  const dx = w / Math.max(values.length - 1, 1);
  const pts = values.map((v, i) => {
    const x = i * dx;
    const y = h - ((v - min) / range) * (h - 4) - 2;
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  });
  return (
    <svg className="sparkline" viewBox={`0 0 ${w} ${h}`}>
      <path d={`M${pts.join(" L")}`} />
    </svg>
  );
}
