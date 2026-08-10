import type { DownloadView } from "../contract";
import { formatSeconds, humanBytes } from "../format";

/**
 * The supervised download's progress.
 *
 * A finished download keeps its bar until another one starts, because the
 * destination path is the useful part and it would otherwise vanish the moment
 * it became relevant.
 */
export function DownloadBar({ download }: { download: DownloadView }) {
  const completed = !download.active && download.completed_path !== null;
  const ratio =
    completed || download.total_bytes === null || download.total_bytes <= 0
      ? completed
        ? 1
        : 0
      : Math.min(1, download.downloaded_bytes / download.total_bytes);

  const detail = completed
    ? `Downloaded: ${download.completed_path ?? ""}`
    : [
        download.total_bytes !== null && download.total_bytes > 0
          ? `${(ratio * 100).toFixed(1)}% · ${humanBytes(download.downloaded_bytes)} / ${humanBytes(download.total_bytes)}`
          : `${humanBytes(download.downloaded_bytes)} · size unknown`,
        download.bytes_per_second === null ? null : `${humanBytes(download.bytes_per_second)}/s`,
        download.eta_seconds === null ? null : `${formatSeconds(download.eta_seconds)} left`,
      ]
        .filter((part): part is string => part !== null)
        .join(" · ");

  return (
    <div className="border-t border-line bg-surface px-[18px] py-[7px]" aria-label="Download">
      <div className="flex items-baseline justify-between gap-4 text-[11px]">
        <span className="min-w-0 truncate text-ink-dim">{download.title}</span>
        <span className="shrink-0 font-mono tabular-nums text-ink-faint">{detail}</span>
      </div>
      <div className="mt-[4px] h-[3px] rounded-[3px] bg-line-strong">
        <i
          className="block h-full rounded-[3px] bg-accent"
          style={{ width: `${ratio * 100}%` }}
        />
      </div>
    </div>
  );
}
