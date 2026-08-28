/** Fixed virtual subscription-row height in pixels. */
export const SUBSCRIPTION_ROW_HEIGHT = 48;

/**
 * Converts one subscription pane's usable viewport height into full rows.
 *
 * The scrolling element has no vertical padding, so `clientHeight` is the
 * exact space available to these fixed-height virtual rows.
 */
export function subscriptionPageRows(viewportHeight: number): number | null {
	return viewportHeight > 0
		? Math.max(1, Math.floor(viewportHeight / SUBSCRIPTION_ROW_HEIGHT))
		: null;
}
