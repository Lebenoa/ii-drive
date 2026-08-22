/** Humanize bytes in binary units: B / KiB / MiB / GiB / TiB */
export function humanSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  const s = u === 0 ? String(v) : v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2);
  return `${s} ${units[u]}`;
}

const MIN = 60;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;

/** Relative time ("2h ago"); absolute date fallback beyond ~30 days or future dates. */
export function relTime(unixSec: number, now = Date.now() / 1000): string {
  const diff = now - unixSec;
  if (!Number.isFinite(diff)) return '—';
  if (diff < -DAY || diff > 30 * DAY) {
    const d = new Date(unixSec * 1000);
    return d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
  }
  if (diff < 0 || diff < MIN) return 'just now';
  if (diff < HOUR) return `${Math.floor(diff / MIN)}m ago`;
  if (diff < DAY) return `${Math.floor(diff / HOUR)}h ago`;
  return `${Math.floor(diff / DAY)}d ago`;
}

/** Type icon glyph by mime type / extension. */
export function mimeIcon(mime: string, name = ''): string {
  const m = (mime || '').toLowerCase();
  const ext = name.includes('.') ? name.slice(name.lastIndexOf('.') + 1).toLowerCase() : '';

  if (m.startsWith('image/')) return '🖼';
  if (m.startsWith('video/')) return '🎬';
  if (m.startsWith('audio/')) return '🔊';

  const archives = ['zip', 'rar', '7z', 'gz', 'tar'];
  if (
    m === 'application/zip' ||
    m === 'application/x-zip-compressed' ||
    m === 'application/x-rar-compressed' ||
    m === 'application/vnd.rar' ||
    m === 'application/x-7z-compressed' ||
    m === 'application/gzip' ||
    m === 'application/x-gzip' ||
    m === 'application/x-tar' ||
    m === 'application/octet-stream' && archives.includes(ext)
  ) {
    return '📦';
  }

  if (
    m.startsWith('text/') ||
    m.includes('json') ||
    m.includes('xml') ||
    m.includes('javascript') ||
    ['ts', 'js', 'rs', 'py', 'toml', 'yaml', 'yml', 'md'].includes(ext)
  ) {
    return '📝';
  }

  return '📄';
}
