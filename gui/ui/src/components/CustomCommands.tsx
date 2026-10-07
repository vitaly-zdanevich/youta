import type { CustomCommandButtonView, CustomCommandOutputView } from '../contract';
import { dispatch } from '../ipc';
import { Popup, PopupButton } from './Popup';
import { LAYER } from './popups';

/** Render only the controller's eligible buttons; command source never reaches the window. */
export function CustomCommandButtons({ buttons }: { buttons: CustomCommandButtonView[] }) {
	return <>
		{buttons.map((button) => <button
			key={button.id}
			type='button'
			onClick={() => void dispatch({ RunCustomCommand: button.id })}
			style={{ color: button.font_color ?? undefined, backgroundColor: button.background_color ?? undefined }}
			className='rounded-[5px] border border-line-strong px-[8px] py-[3px] text-[11px] whitespace-nowrap text-ink-dim hover:border-ink-faint focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent'
		>
			{button.hotkey ? `[${button.hotkey}] ` : ''}{button.name}
		</button>)}
	</>;
}

/** Show bounded output as text, retaining modal ownership until the worker finishes. */
export function CustomCommandOutputPopup({ popup }: { popup: CustomCommandOutputView }) {
	const dismiss = () => {
		if (!popup.running) void dispatch('DismissCustomCommandOutput');
	};
	return <Popup
		title={popup.name}
		layer={LAYER.customCommand}
		dismissDisabled={popup.running}
		onDismiss={dismiss}
		footer={<PopupButton disabled={popup.running} onClick={dismiss}>Close</PopupButton>}
	>
		<div className='max-h-[65vh] overflow-auto px-[18px] py-[11px] text-xs'>
			{popup.running ? <p role='status' className='flex items-center gap-2 text-ink-dim'>
				<span aria-hidden='true' className='inline-block animate-spin'>|</span>
				Running...
			</p> : <>
				<p role='status' className={popup.failed ? 'mb-3 text-red-400' : 'mb-3 text-ink-dim'}>
					{popup.failed ? 'Command failed.' : 'Command finished.'}
				</p>
				<pre className='font-mono whitespace-pre-wrap break-words' data-custom-command-output>
					{popup.output || 'No output.'}
				</pre>
			</>}
		</div>
	</Popup>;
}
