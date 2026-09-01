import type { DownloadView } from "../contract";
import { formatSeconds, humanBytes } from "../format";
import { dispatch } from '../ipc';

/** Computes whole-collection progress while preserving current-file detail. */
function downloadRatio(download: DownloadView, completed: boolean): number {
	if (completed) {
		return 1;
	}
	if (download.collection && download.total_files !== null && download.total_files > 0) {
		const currentFileRatio = download.total_bytes === null || download.total_bytes <= 0
			? 0
			: Math.min(1, download.downloaded_bytes / download.total_bytes);
		const priorFiles = Math.max(
			download.completed_files,
			(download.current_file ?? download.completed_files + 1) - 1,
		);
		return Math.min(1, (priorFiles + currentFileRatio) / download.total_files);
	}
	return download.total_bytes === null || download.total_bytes <= 0
		? 0
		: Math.min(1, download.downloaded_bytes / download.total_bytes);
}

/**
 * The supervised download's progress.
 *
 * A finished download keeps its bar until another one starts, because the
 * destination path is the useful part and it would otherwise vanish the moment
 * it became relevant.
 */
export function DownloadBar({ download }: { download: DownloadView }) {
  const completed = !download.active && download.completed_path !== null;
  const ratio = downloadRatio(download, completed);

  const detail = completed
    ? download.collection
		? download.completed_files === 0
			? `Channel already up to date: ${download.completed_path ?? ''}`
			: `Downloaded ${download.completed_files} files: ${download.completed_path ?? ''}`
		: `Downloaded: ${download.completed_path ?? ""}`
    : [
		download.collection
			? download.current_file === null
				? download.total_files === null
					? 'enumerating items'
					: `0 / ${download.total_files} items`
				: download.total_files === null
					? `item ${download.current_file}`
					: `item ${download.current_file} / ${download.total_files}`
			: null,
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
		{download.active ? (
			<button
				type='button'
				onClick={() => void dispatch("CancelDownload")}
				className='shrink-0 rounded-[5px] border border-line-strong px-[7px] py-[2px] text-ink-dim hover:border-ink-faint hover:text-ink'
			>
				Cancel
			</button>
		) : null}
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
