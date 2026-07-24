export type SparklinePoint = {
  timestamp: number;
  value: number;
};

/** Renders timestamped numeric samples over a fixed time window. */
export function Sparkline({
  label,
  points,
  windowMs,
  minimum,
  maximum,
  className,
}: {
  label: string;
  points: readonly SparklinePoint[];
  windowMs: number;
  minimum?: number;
  maximum?: number;
  className?: string;
}) {
  const samples = points.filter((point) =>
    Number.isFinite(point.value) && Number.isFinite(point.timestamp)
  );
  const sparklineClassName = `block ${className ?? ""}`;
  if (samples.length < 2) return <span className={sparklineClassName} aria-hidden="true" />;

  const width = 100;
  const height = 28;
  const inset = 1;
  const latestTimestamp = samples.at(-1)?.timestamp ?? 0;
  const windowStart = latestTimestamp - windowMs;
  const lowerBound = minimum ?? Math.min(...samples.map((sample) => sample.value));
  const upperBound = maximum ?? Math.max(...samples.map((sample) => sample.value));
  const range = upperBound - lowerBound;
  const coordinates = samples.map(({ timestamp, value }) => {
    const x = Math.min(width, Math.max(0, (timestamp - windowStart) / windowMs * width));
    const boundedValue = Math.min(upperBound, Math.max(lowerBound, value));
    const y = range === 0
      ? height / 2
      : inset + (upperBound - boundedValue) / range * (height - inset * 2);
    return [x, y] as const;
  });
  const line = coordinates
    .map(([x, y], index) => `${index === 0 ? "M" : "L"}${x.toFixed(2)},${y.toFixed(2)}`)
    .join(" ");
  const firstX = coordinates[0]?.[0] ?? 0;
  const lastX = coordinates.at(-1)?.[0] ?? width;
  const area = `${line} L${lastX.toFixed(2)},${height} L${firstX.toFixed(2)},${height} Z`;

  return (
    <svg
      className={sparklineClassName}
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      role="img"
      aria-label={label}
    >
      <path d={area} fill="currentColor" opacity="0.08" />
      <path
        d={line}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}
