/** The GUI capture boundary never reads private editor DOM or sends unbounded text. */
import assert from 'node:assert/strict';
import test from 'node:test';
import { captureBugReportScreenshot, visibleWindowText } from './bugReport.ts';

test('bug report captures a labeled GUI text snapshot before opening', () => {
	let reads = 0;
	const screenshot = captureBugReportScreenshot({}, () => { reads++; return 'YouTube\nSelected video'; });
	assert.equal(reads, 1);
	assert.match(screenshot, /GUI text snapshot/);
	assert.match(screenshot, /YouTube\nSelected video/);
});

test('bug report never reads private, credential, or existing report drafts', () => {
	for (const field of ['bug_report_popup', 'private_note_open', 'rss_subscription_open',
		'yandex_music_setup_open', 'youtube_provider_editor', 'commons_credentials_editor',
		'evernote_credentials_editor', 's3_credentials_editor', 'archive_credentials_editor',
		'evernote_popup', 'commons_upload_popup', 's3_upload_popup', 'archive_upload_popup',
		'local_file_popup', 'local_file_progress', 'preferences_popup']) {
		assert.equal(captureBugReportScreenshot({ [field]: true }, () => {
			throw new Error(`Private DOM was read: ${field}`);
		}), null);
	}
	for (const mode of ['Create', 'Edit']) {
		assert.equal(captureBugReportScreenshot({ playlist_popup: { mode } }, () => {
			throw new Error('Playlist draft DOM was read');
		}), null);
	}
});

test('bug report GUI capture is byte bounded and survives unavailable DOM text', () => {
	const screenshot = captureBugReportScreenshot({}, () => '📮'.repeat(40_000));
	assert.ok(new TextEncoder().encode(screenshot).length <= 32 * 1024);
	assert.ok(!screenshot.includes('\uFFFD'));
	assert.ok(screenshot.endsWith('[Screenshot truncated]'));
	assert.equal(captureBugReportScreenshot({}, () => { throw new Error('DOM unavailable'); }), null);
});

test('visible GUI capture marks DOM traversal limits without reading beyond them', () => {
	const prior = globalThis.NodeFilter;
	globalThis.NodeFilter = { SHOW_TEXT: 4 };
	try {
		const parent = { parentElement: null, closest: () => false,
			getBoundingClientRect: () => ({ left: 0, top: 0, right: 100, bottom: 100 }) };
		const document = {
			body: {},
			defaultView: { innerWidth: 100, innerHeight: 100,
				getComputedStyle: () => ({ visibility: 'visible', display: 'block', overflowX: 'visible', overflowY: 'visible' }) },
			createTreeWalker: () => ({ nextNode: () => true, currentNode: { parentElement: parent, textContent: 'x' } }),
			createRange: () => ({ selectNodeContents: () => {},
				getClientRects: () => [{ left: 1, top: 1, right: 2, bottom: 2, width: 1, height: 1 }] }),
		};
		assert.ok(visibleWindowText(document).endsWith('[Screenshot truncated]'));
	} finally {
		globalThis.NodeFilter = prior;
	}
});
