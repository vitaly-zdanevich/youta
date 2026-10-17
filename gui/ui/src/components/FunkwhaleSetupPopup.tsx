import { useCallback, useEffect, useRef, useState } from 'react';
import type { KeyboardEvent, ReactNode } from 'react';

import type { UiAction } from '../actions';
import type { FunkwhaleSetupPopupView } from '../contract';
import { dispatch } from '../ipc';
import { isTopmostPopup, Popup, PopupButton, PopupError } from './Popup';
import { LAYER } from './popups';

/** Explicit controls consume activation once instead of also invoking the modal keymap. */
function activateKey(event: KeyboardEvent, action: UiAction) {
	if (event.ctrlKey || event.altKey || event.metaKey || (event.key !== 'Enter' && event.key !== ' ')) return;
	event.preventDefault();
	event.stopPropagation();
	if (!event.repeat) void dispatch(action);
}

/** Reuse popup styling while keeping each button's keyboard action identical to its click. */
function SetupButton({ children, action, emphasis = false, disabled = false }: {
	children: ReactNode;
	action: UiAction;
	emphasis?: boolean;
	disabled?: boolean;
}) {
	return <PopupButton emphasis={emphasis} disabled={disabled}
		onClick={() => void dispatch(action)}
		onKeyDown={(event) => { if (!disabled) activateKey(event, action); }}>
		{children}
	</PopupButton>;
}

/**
 * Pod setup with a local text draft and native directory/save ownership.
 *
 * The core omits invalid URL drafts from snapshots, so a partial edit cannot be
 * reconstructed from its safe projection. Native validation still receives each
 * edit. Selecting a public pod replaces the draft; only Save persists the URL.
 */
export function FunkwhaleSetupPopup({ popup }: { popup: FunkwhaleSetupPopupView }) {
	const picker = popup.picker;
	const picking = picker !== null;
	const [draft, setDraft] = useState(popup.url);
	const editing = useRef(false);
	const input = useRef<HTMLInputElement>(null);
	const selected = useRef<HTMLButtonElement>(null);
	const previousPicking = useRef(picking);
	/** Portals can attach after parent effects; only mount or picker transitions claim focus. */
	const attachInput = useCallback((node: HTMLInputElement | null) => {
		input.current = node;
		if (node && !picking) node.focus();
	}, [picking]);
	useEffect(() => {
		const pickerClosed = previousPicking.current && !picking;
		previousPicking.current = picking;
		if (picking) {
			editing.current = false;
		} else if (popup.url && (!editing.current || pickerClosed)) {
			// Ref focus precedes effects, but an explicit pod selection still owns the new draft.
			setDraft(popup.url);
		}
	}, [popup.url, picking]);
	useEffect(() => {
		selected.current?.scrollIntoView({ block: 'nearest' });
		selected.current?.focus();
	}, [picking, picker?.selected, picker?.loading]);

	/** Text editing remains native; Save, Escape and clear preserve reducer semantics. */
	const inputKey = (event: KeyboardEvent<HTMLInputElement>) => {
		if (event.metaKey || event.altKey) return;
		let action: UiAction;
		if (event.ctrlKey && event.key.toLowerCase() === 'u') {
			setDraft('');
			action = 'ClearFunkwhalePodUrl';
		} else if (!event.ctrlKey && event.key === 'Enter') {
			action = 'SaveFunkwhaleSetup';
		} else if (!event.ctrlKey && event.key === 'Escape') {
			action = 'CloseFunkwhaleSetup';
		} else {
			return;
		}
		event.preventDefault();
		event.stopPropagation();
		if (!event.repeat) void dispatch(action);
	};

	/** Navigation changes only the native highlight; activation fills without persisting. */
	const podKey = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
		if (event.ctrlKey || event.altKey || event.metaKey) return;
		if (event.key === 'ArrowDown' || event.key === 'ArrowUp' || event.key === 'Escape') {
			event.preventDefault();
			event.stopPropagation();
			void dispatch(event.key === 'Escape' ? 'CloseFunkwhalePodPicker'
				: { MoveFunkwhalePod: event.key === 'ArrowDown' ? 1 : -1 });
		} else {
			activateKey(event, { SelectFunkwhalePod: index });
		}
	};

	/** Native focus stays inside setup; button-focused typing must update the same local draft. */
	const dialogKey = (event: KeyboardEvent<HTMLDivElement>) => {
		if (event.metaKey || event.altKey) return;
		// Covered header activation belongs to the upper popup, never the parked URL draft.
		if (!isTopmostPopup(event.currentTarget)) return;
		if (event.key === 'Tab' && !event.ctrlKey) {
			event.stopPropagation();
			const controls = [...event.currentTarget.querySelectorAll<HTMLElement>('button:not([disabled]), input:not([disabled]), [tabindex="0"]')]
				.filter((control) => control.tabIndex >= 0);
			const first = controls[0];
			const last = controls.at(-1);
			if ((!event.shiftKey && event.target === last) || (event.shiftKey && event.target === first)) {
				event.preventDefault();
				(event.shiftKey ? last : first)?.focus();
			}
			return;
		}
		if (picking || event.target === input.current) return;
		if (event.ctrlKey && event.key.toLowerCase() === 'u') {
			setDraft('');
			void dispatch('ClearFunkwhalePodUrl');
		} else if (!event.ctrlKey && event.key === 'Backspace') {
			const last = [...new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(draft)].at(-1)?.index ?? 0;
			const next = draft.slice(0, last);
			setDraft(next);
			void dispatch({ SetFunkwhalePodUrl: next });
		} else if (!event.ctrlKey && [...event.key].length === 1 && (event.key.codePointAt(0) ?? 0) >= 32) {
			const next = draft + event.key;
			setDraft(next);
			void dispatch({ SetFunkwhalePodUrl: next });
		} else {
			return;
		}
		event.preventDefault();
		event.stopPropagation();
		input.current?.focus();
	};

	return <Popup title='Funkwhale pod setup' layer={LAYER.credentialEditor} width='620px'
		onKeyDown={dialogKey}
		onDismiss={() => void dispatch(picking ? 'CloseFunkwhalePodPicker' : 'CloseFunkwhaleSetup')}
		dismissLabel={picking ? 'Close pod list' : 'Cancel'}
		footer={<>
			<SetupButton action='SaveFunkwhaleSetup' emphasis disabled={picking}>Save</SetupButton>
			<SetupButton action='CloseFunkwhaleSetup'>Cancel</SetupButton>
		</>}>
		<div className='grid max-h-[65vh] min-w-0 gap-3 overflow-y-auto px-[18px] py-[11px] text-xs'>
			<p className='m-0 text-ink-dim'>Choose a public pod or enter your own pod URL. Leave the URL empty and save to remove the saved pod.</p>
			<label className='grid min-w-0 gap-1 text-ink-faint'>
				Pod URL
				<input ref={attachInput} type='url' value={draft} disabled={picking}
					placeholder='https://your-pod.example/' autoComplete='off' autoCapitalize='none' spellCheck={false}
					onFocus={() => { editing.current = true; }}
					onBlur={() => { editing.current = false; }}
					onChange={(event) => {
						setDraft(event.currentTarget.value);
						void dispatch({ SetFunkwhalePodUrl: event.currentTarget.value });
					}}
					onKeyDown={inputKey}
					className='min-w-0 rounded-[5px] border border-line-strong bg-raised px-[9px] py-[7px] text-xs text-ink disabled:opacity-50 focus-visible:outline-2 focus-visible:outline-accent' />
			</label>
			<div className='flex flex-wrap gap-2'>
				<SetupButton action='OpenFunkwhalePodPicker'>{picking ? 'Retry' : 'Choose public pod'}</SetupButton>
				{picking ? <SetupButton action='CloseFunkwhalePodPicker'>Close pod list</SetupButton> : null}
			</div>
			{picker ? <section aria-label='Public Funkwhale pods' className='grid min-w-0 gap-2 rounded-[5px] border border-line-strong p-2'>
				<p className='m-0 text-ink-faint'>Up/Down selects; Enter fills the URL. Esc closes this list.</p>
				{picker.loading ? <p role='status' className='m-0 text-ink-dim'>Loading public pods...</p>
					: picker.error ? <p role='alert' className='m-0 whitespace-pre-wrap break-words text-accent'>{picker.error}</p>
					: picker.pods.length === 0 ? <p className='m-0 text-ink-dim'>No public pods are available. Retry or enter a URL manually.</p>
					: <div role='listbox' aria-label='Funkwhale pods' className='max-h-[220px] min-w-0 overflow-y-auto'>
						{picker.pods.map((pod, index) => <button key={pod.url} type='button' role='option'
							aria-selected={index === picker.selected} tabIndex={index === picker.selected ? 0 : -1}
							ref={index === picker.selected ? selected : undefined}
							onClick={() => void dispatch({ SelectFunkwhalePod: index })}
							onKeyDown={(event) => podKey(event, index)}
							className={`grid w-full min-w-0 gap-1 rounded px-2 py-2 text-left focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent ${index === picker.selected ? 'bg-raised text-ink' : 'text-ink-dim hover:bg-raised'}`}>
							<span className='break-words'>{pod.label || pod.url}</span>
							<span className='break-all text-ink-faint'>{pod.url}</span>
							{pod.open_registrations !== null ? <span className='text-[11px] text-ink-faint'>
								{pod.open_registrations ? 'Registration open' : 'Registration closed'}
							</span> : null}
						</button>)}
					</div>}
			</section> : null}
			<PopupError message={popup.error} />
			<p className='m-0 break-all text-[11px] text-ink-faint'>Save to {popup.config_file_path}</p>
		</div>
	</Popup>;
}
