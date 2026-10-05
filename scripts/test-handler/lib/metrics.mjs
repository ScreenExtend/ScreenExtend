// Distribution helpers. Percentiles are nearest-rank on the sorted samples.
export function pct(sorted, p) {
  if (!sorted.length) return null;
  const i = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[i];
}
const r2 = (x) => (x == null || Number.isNaN(x) ? null : Math.round(x * 100) / 100);

export function dist(values) {
  const v = values.filter((x) => typeof x === 'number' && Number.isFinite(x)).sort((a, b) => a - b);
  if (!v.length) return { n: 0 };
  const mean = v.reduce((a, b) => a + b, 0) / v.length;
  return { n: v.length, min: r2(v[0]), p50: r2(pct(v, 50)), p90: r2(pct(v, 90)), p99: r2(pct(v, 99)), max: r2(v[v.length - 1]), mean: r2(mean) };
}

/** Median, range and spread across runs for one numeric field. */
export function spread(values) {
  const v = values.filter((x) => typeof x === 'number' && Number.isFinite(x)).sort((a, b) => a - b);
  if (!v.length) return null;
  return { runs: v.length, median: r2(pct(v, 50)), min: r2(v[0]), max: r2(v[v.length - 1]), range: r2(v[v.length - 1] - v[0]), values: v.map(r2) };
}

export { r2 };
