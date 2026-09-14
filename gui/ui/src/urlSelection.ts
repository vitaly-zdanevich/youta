/**
 * Preserves original URL bytes when copying a selection across a description.
 *
 * Replacements are complete graphemes. Even a partial selection of one restores
 * its complete encoded source, not a broken UTF-8 escape. A selection may start
 * or end outside the description; only mapped URL spans are replaced. Selections
 * without them keep the browser's normal clipboard behavior and rich formatting.
 */
export function copyOriginalDescriptionUrls(root: HTMLElement, event: ClipboardEvent): void {
	const target = event.target;
	if (event.defaultPrevented || target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement
		|| (target instanceof HTMLElement && target.isContentEditable)) return;
	const selection = root.ownerDocument.getSelection();
	if (!event.clipboardData || !selection || selection.rangeCount !== 1 || selection.isCollapsed) return;
	const range = selection.getRangeAt(0);
	if (!range.intersectsNode(root)) return;
	const ancestor = range.commonAncestorContainer;
	const element = ancestor instanceof Element ? ancestor : ancestor.parentElement;
	const single = element?.closest<HTMLElement>('[data-url-escape-original]');
	let original: string;
	if (single && root.contains(single)) {
		original = single.dataset.urlEscapeOriginal!;
	} else {
		const fragment = range.cloneContents();
		const replacements = fragment.querySelectorAll<HTMLElement>('[data-url-escape-original]');
		if (replacements.length === 0) return;
		for (const replacement of replacements) replacement.replaceWith(replacement.dataset.urlEscapeOriginal!);
		original = fragment.textContent ?? '';
	}
	event.clipboardData.setData('text/plain', original);
	event.preventDefault();
}
