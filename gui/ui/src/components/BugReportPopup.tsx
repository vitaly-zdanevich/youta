import { useEffect, useRef } from 'react';
import type { BugReportField, BugReportPopupView } from '../contract';
import { dispatch } from '../ipc';
import { Popup, PopupButton, PopupError } from './Popup';
import { splitAtByte } from './SearchBar';

/** Render the controller's UTF-8 byte cursor without interpreting authored markup. */
function CursorText({ text, cursor, active }: { text: string; cursor: number; active: boolean }) {
	const [before, after] = splitAtByte(text, cursor);
	return <>{before}{active
		? <span data-bug-report-cursor className='inline-block h-3 border-l-2 border-accent' aria-hidden='true' /> : null}{after}</>;
}

/** Direct, explicitly public submission; Rust owns editing, duplicate guards and all effects. */
export function BugReportPopup({ popup, externalOpener }: {
	popup: BugReportPopupView;
	externalOpener: boolean;
}) {
	const body = useRef<HTMLButtonElement>(null);
	const pending = popup.submission === 'Submitting';
	const failed = typeof popup.submission === 'object' && 'Failed' in popup.submission
		? popup.submission.Failed.message : null;
	const editable = popup.submission === 'Idle' || failed !== null;
	const submitted = typeof popup.submission === 'object' && 'Submitted' in popup.submission
		? popup.submission.Submitted.url : null;
	const unknown = typeof popup.submission === 'object' && 'OutcomeUnknown' in popup.submission
		? popup.submission.OutcomeUnknown.issues_url : null;
	useEffect(() => {
		const element = body.current;
		if (!element) return;
		if (popup.follow_cursor && popup.selected_field === 'Body') {
			element.querySelector('[data-bug-report-cursor]')?.scrollIntoView({ block: 'nearest' });
		} else {
			element.scrollTop = popup.body_scroll_offset * 18;
		}
	}, [popup.body_cursor_byte, popup.body_scroll_offset, popup.follow_cursor, popup.selected_field]);
	const field = (name: BugReportField, text: string, cursor: number) => <button
		key={name} type='button' aria-label={name} disabled={!editable}
		ref={name === 'Body' ? body : undefined}
		onClick={() => void dispatch({ SelectBugReportField: name })}
		className={`min-h-[52px] w-full overflow-y-auto rounded-[6px] border px-3 py-2 text-left text-xs leading-[18px] disabled:opacity-70 ${name === 'Body' ? 'max-h-[240px]' : ''} ${popup.selected_field === name ? 'border-accent' : 'border-line-strong'}`}>
		<span className='block text-[11px] text-ink-faint'>{name}</span>
		<span className='block break-words whitespace-pre-wrap'><CursorText text={text} cursor={cursor}
			active={editable && popup.selected_field === name} /></span>
	</button>;
	return <Popup title='Report a bug' layer={17} width='760px' dismissDisabled={pending}
		onDismiss={() => void dispatch('DismissBugReport')}
		footer={<>
			<PopupButton emphasis={!pending} disabled={!editable || !popup.gh_available}
				onClick={() => void dispatch('SubmitBugReport')}>Submit</PopupButton>
			{pending ? <span role='status' aria-live='polite' className='font-mono text-ink-dim'>
				{['|', '/', '-', '\\'][popup.animation_frame % 4]} Submitting...
			</span> : null}
			<PopupButton disabled={pending} onClick={() => void dispatch('CopyBugReport')}>Copy report</PopupButton>
			{(submitted || unknown) && externalOpener
				? <PopupButton onClick={() => void dispatch('OpenBugReportResult')}>{submitted ? 'Open issue' : 'Check GitHub issues'}</PopupButton> : null}
			<PopupButton disabled={pending} onClick={() => void dispatch('DismissBugReport')}>Close</PopupButton>
			<span>Tab: field · Ctrl+S / Ctrl+Enter: submit · Esc: close</span>
		</>}>
		<div className='grid max-h-[70vh] gap-3 overflow-y-auto px-[18px] py-3'>
			<p className='m-0 text-xs text-ink-dim'>This report will be published publicly on GitHub. Remove private information before submitting.</p>
			{field('Title', popup.title, popup.title_cursor_byte)}
			{field('Body', popup.body, popup.body_cursor_byte)}
			<button type='button' role='checkbox' aria-checked={popup.with_screenshot} disabled={!editable || !popup.screenshot_available}
				onClick={() => void dispatch('ToggleBugReportScreenshot')}
				className={`rounded-[6px] border px-3 py-2 text-left text-xs disabled:opacity-70 ${popup.selected_field === 'Screenshot' ? 'border-accent' : 'border-line-strong'}`}>
				[{popup.with_screenshot ? 'x' : ' '}] With ASCII screenshot
			</button>
			<p className='m-0 text-[11px] text-ink-faint'>{popup.screenshot_notice
				?? (popup.screenshot_available ? 'The GUI capture is a text-only snapshot, without artwork.' : 'No screen capture is available for this report.')}</p>
			<pre className='m-0 whitespace-pre-wrap text-[11px] text-ink-dim'>{popup.footer}</pre>
			{!popup.gh_available ? <p className='m-0 text-xs text-ink-dim'>Install GitHub CLI and run gh auth login to submit here, or copy the report.</p> : null}
			{submitted ? <p role='status' className='m-0 break-all text-xs'>Submitted to GitHub: {submitted}</p> : null}
			{unknown ? <p role='status' className='m-0 break-all text-xs'>The submission outcome is unknown. Check GitHub before creating another issue: {unknown}</p> : null}
			<PopupError message={popup.validation_error ?? failed} />
		</div>
	</Popup>;
}
