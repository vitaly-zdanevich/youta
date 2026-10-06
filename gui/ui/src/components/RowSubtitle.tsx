import type { RowView } from '../contract';

/** Color only the controller's canonical live duration, preserving channel text verbatim. */
export function RowSubtitle({ row }: { row: RowView }) {
	if (!row.live || (row.subtitle !== 'LIVE' && !row.subtitle.endsWith(' · LIVE'))) {
		return row.subtitle;
	}
	return <>{row.subtitle.slice(0, -4)}<span className='font-semibold text-red-400'>LIVE</span></>;
}
