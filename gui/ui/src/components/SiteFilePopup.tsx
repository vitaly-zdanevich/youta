import { useEffect, useRef } from 'react';
import type { KeyboardEvent, ReactNode } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';

import type { SiteFilePopupView } from '../contract';
import type { UiAction } from '../actions';
import { dispatch } from '../ipc';
import { reportEntryGeometry, reportGeometry } from '../popupGeometry';
import { Popup, PopupError } from './Popup';
import { ScrollingText } from './ScrollingText';
import { LAYER } from './popups';

/** Explicit controls own activation keys so one event cannot also activate a selected row. */
function activateKey(event: KeyboardEvent, action: UiAction) {
	if (event.ctrlKey || event.altKey || event.metaKey || (event.key !== 'Enter' && event.key !== ' ')) return;
	event.preventDefault();
	event.stopPropagation();
	if (!event.repeat) void dispatch(action);
}

/** Small navigation controls dispatch only semantic core actions, never raw URLs. */
function SiteFileButton({ children, action }: { children: ReactNode; action: UiAction }) {
	return <button type='button'
		onClick={(event) => { event.stopPropagation(); void dispatch(action); }}
		onKeyDown={(event) => activateKey(event, action)}
		className='rounded-[5px] border border-line-strong px-[9px] py-[3px] text-[11px] text-ink-dim hover:border-ink-faint focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent'>
		{children}
	</button>;
}

/** Virtualized variable-height entries keep large site maps responsive without dropping metadata. */
function SitemapEntries({ popup }: { popup: SiteFilePopupView }) {
	const body = useRef<HTMLDivElement>(null);
	const lastOffset = useRef<number | null>(null);
	const virtualizer = useVirtualizer({
		count: popup.entries.length,
		getScrollElement: () => body.current,
		estimateSize: (index) => 34 + (popup.entries[index]?.metadata.length ?? 0) * 17,
		overscan: 4,
		onChange: (instance) => {
			const offset = instance.scrollOffset ?? 0;
			const height = instance.scrollRect?.height ?? 0;
			const visible = instance.getVirtualItems().filter((item) => item.end > offset && item.start < offset + height);
			const first = visible[0]?.index ?? 0;
			reportEntryGeometry('site_file', first, popup.entries.length, visible.length);
			// Scroll gestures publish the first row; the effect below will not snap them back.
			if (instance.isScrolling && lastOffset.current !== first) {
				lastOffset.current = first;
				void dispatch({ SetSiteFileScroll: first });
			}
		},
	});

	useEffect(() => {
		if (lastOffset.current !== popup.scroll_offset && popup.entries.length > 0) {
			lastOffset.current = popup.scroll_offset;
			virtualizer.scrollToIndex(popup.scroll_offset, { align: 'start' });
		}
	}, [popup.scroll_offset, popup.entries.length, virtualizer]);
	useEffect(() => () => reportGeometry('site_file', null, 0), []);

	return <div ref={body} aria-label={popup.sitemap_index ? 'Child sitemaps' : 'Sitemap pages'}
		className='h-[55vh] overflow-y-auto px-[18px] text-xs'>
		<div className='relative w-full' style={{ height: virtualizer.getTotalSize() }}>
			{virtualizer.getVirtualItems().map((item) => {
				const entry = popup.entries[item.index];
				if (!entry) return null;
				const action: UiAction = { ActivateSiteFileEntry: item.index };
				return <button key={item.key} type='button' ref={virtualizer.measureElement}
					data-index={item.index} data-site-file-entry={item.index} aria-current={item.index === popup.selected}
					title={popup.sitemap_index ? 'Open child sitemap in Youta' : 'Open page in Youta Web tab'}
					onClick={(event) => { event.stopPropagation(); void dispatch(action); }}
					onKeyDown={(event) => activateKey(event, action)}
					className={`absolute top-0 left-0 w-full rounded-[5px] px-2 py-[8px] text-left focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent ${item.index === popup.selected ? 'bg-raised' : ''}`}
					style={{ transform: `translateY(${item.start}px)` }}>
					<span className='block break-all font-mono text-accent'>{entry.url}</span>
					{entry.metadata.map(([label, value], index) => <span key={index} className='block whitespace-pre-wrap break-words text-ink-faint'>
						{label}: {value}
					</span>)}
				</button>;
			})}
		</div>
	</div>;
}

/** Displays reducer-owned site files without frontend requests, HTML rendering or browser links. */
export function SiteFilePopup({ popup }: { popup: SiteFilePopupView }) {
	return <Popup title={popup.title} subtitle={popup.url} layer={LAYER.siteFile}
		onDismiss={() => void dispatch('DismissSiteFile')}
		footer={<>
			{popup.can_go_back ? <SiteFileButton action='BackSiteFile'>Alt+Left Back</SiteFileButton> : null}
			<SiteFileButton action='DismissSiteFile'>Close</SiteFileButton>
			{popup.sitemap && !popup.loading && (!popup.error || popup.entries.length > 0) ? <span>
				{popup.entries.length} {popup.sitemap_index ? 'sitemaps' : 'pages'} - Enter {popup.sitemap_index ? 'opens a child sitemap' : 'opens the page in Youta Web'}
			</span> : null}
		</>}>
		<PopupError message={popup.error} />
		{popup.loading ? <p role='status' className='px-[18px] py-[11px] text-xs text-ink-faint'>Loading...</p>
			: popup.sitemap && popup.entries.length > 0 ? <SitemapEntries key={popup.url} popup={popup} />
				: popup.error ? null
				: popup.sitemap ? <p className='px-[18px] py-[11px] text-xs text-ink-faint'>No sitemap entries.</p>
			: <div className='h-[55vh]'>
				<ScrollingText popup='site_file' offset={popup.scroll_offset}
					onScroll={(offset) => void dispatch({ SetSiteFileScroll: offset })}>
					{popup.text}
				</ScrollingText>
			</div>}
	</Popup>;
}
