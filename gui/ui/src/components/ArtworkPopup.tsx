import type { DetailView } from '../contract';
import { dispatch } from '../ipc';
import { Artwork } from './Artwork';
import { Popup } from './Popup';

/**
 * Opens the selected cached artwork rendition, fetching it on demand if needed.
 *
 * The controller owns expansion and Escape handling. Keeping this layer below
 * other popups preserves their keyboard priority, just as in the terminal.
 */
export function ArtworkPopup({ details }: { details: DetailView | null }) {
	const url = details?.expanded_thumbnail_url ?? details?.thumbnail_url;
	if (!details?.thumbnail_expanded || !url) return null;
	const collapse = () => void dispatch('ToggleThumbnailExpansion');
	return (
		<Popup title='Artwork' subtitle={details.title} width='100%' layer={-1}
			onDismiss={collapse} dismissLabel='Close artwork'>
			<button type='button' aria-label='Collapse artwork' onClick={collapse}
				className='block h-[calc(100vh-130px)] w-full cursor-zoom-out p-3 focus-visible:outline-2 focus-visible:outline-accent'>
				<Artwork url={url} className='block h-full w-full object-contain' />
			</button>
		</Popup>
	);
}
