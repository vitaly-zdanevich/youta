import type { RowView } from '../contract';

/**
 * Brighten unwatched YouTube-video metadata without changing other providers.
 * YouTube Music shares media identities, and accepted zero-percent playback
 * already counts as started, so both source labels and playback fields matter.
 */
export function rowSubtitleColorClass(row: RowView): string {
	return row.media_id?.source === 'you-tube'
		&& row.source === 'YouTube'
		&& !row.playback_started
		&& row.watched_percent === 0
		? 'text-ink-dim'
		: 'text-ink-faint';
}

/** Color only the controller's canonical live duration, preserving channel text verbatim. */
export function RowSubtitle({ row }: { row: RowView }) {
	if (!row.live || (row.subtitle !== 'LIVE' && !row.subtitle.endsWith(' · LIVE'))) {
		return row.subtitle;
	}
	return <>{row.subtitle.slice(0, -4)}<span className='font-semibold text-red-400'>LIVE</span></>;
}
