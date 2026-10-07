import type { ViewModel } from './contract';

/** Maximum IPC capture size; the native controller independently sanitizes and bounds it. */
const MAX_SCREENSHOT_BYTES = 32 * 1024;
const TRUNCATION_NOTICE = '\n[Screenshot truncated]';

/** Capture at opening time only; private editor DOM must never be inspected. */
export function captureBugReportScreenshot(view: ViewModel, readVisibleText: () => string): string | null {
	if (view.bug_report_popup || view.private_note_open || view.rss_subscription_open
		|| view.yandex_music_setup_open || view.youtube_provider_editor
		|| view.commons_credentials_editor || view.evernote_credentials_editor
		|| view.s3_credentials_editor || view.archive_credentials_editor
		|| view.evernote_popup || view.commons_upload_popup || view.s3_upload_popup
		|| view.archive_upload_popup || view.local_file_popup || view.local_file_progress
		|| view.preferences_popup || view.playlist_popup) return null;
	try {
		const text = `GUI text snapshot (text only; no artwork)\n\n${readVisibleText()}`;
		const bytes = new TextEncoder().encode(text);
		if (bytes.length <= MAX_SCREENSHOT_BYTES) return text;
		let end = MAX_SCREENSHOT_BYTES - new TextEncoder().encode(TRUNCATION_NOTICE).length;
		while (end > 0 && ((bytes[end] ?? 0) & 0xc0) === 0x80) end--;
		return new TextDecoder().decode(bytes.subarray(0, end)) + TRUNCATION_NOTICE;
	} catch {
		return null;
	}
}

/** Read rendered, on-screen text only; omit controls' values and offscreen scroll content. */
export function visibleWindowText(document: Document): string {
	const window = document.defaultView;
	if (!window) return '';
	const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
	const lines: string[] = [];
	let length = 0;
	let visited = 0;
	let truncated = false;
	while (walker.nextNode()) {
		if (visited++ >= 8192 || length >= MAX_SCREENSHOT_BYTES) { truncated = true; break; }
		const node = walker.currentNode;
		const parent = node.parentElement;
		const text = node.textContent?.trim();
		if (!parent || !text || parent.closest('script, style, input, textarea, [aria-hidden=true]')) continue;
		const style = window.getComputedStyle(parent);
		if (style.visibility !== 'visible' || style.display === 'none') continue;
		const range = document.createRange();
		range.selectNodeContents(node);
		let left = 0;
		let top = 0;
		let right = window.innerWidth;
		let bottom = window.innerHeight;
		for (let ancestor: HTMLElement | null = parent; ancestor; ancestor = ancestor.parentElement) {
			const ancestorStyle = window.getComputedStyle(ancestor);
			const rect = ancestor.getBoundingClientRect();
			if (ancestorStyle.overflowX !== 'visible') { left = Math.max(left, rect.left); right = Math.min(right, rect.right); }
			if (ancestorStyle.overflowY !== 'visible') { top = Math.max(top, rect.top); bottom = Math.min(bottom, rect.bottom); }
		}
		const rectangles = [...range.getClientRects()];
		if (!rectangles.length || !rectangles.every((rect) => rect.width > 0 && rect.height > 0
			&& rect.left >= left && rect.right <= right && rect.top >= top && rect.bottom <= bottom)) continue;
		let bounded = text.slice(0, MAX_SCREENSHOT_BYTES - length);
		// Do not split the surrogate pair of a final non-BMP character.
		if (/[\uD800-\uDBFF]$/.test(bounded)) bounded = bounded.slice(0, -1);
		truncated ||= bounded.length < text.length;
		lines.push(bounded);
		length += bounded.length + 1;
	}
	return lines.join('\n') + (truncated ? TRUNCATION_NOTICE : '');
}
