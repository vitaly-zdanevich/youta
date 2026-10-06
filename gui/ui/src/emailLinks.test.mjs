/** Email rendering consumes core byte spans and indexed actions, never provider markup or raw URL IPC. */
import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const modules = new Map();
const actions = [];

/** Run the actual JSX and UTF-8 span renderer, replacing only effects, popup chrome and native IPC. */
function load(url) {
	if (modules.has(url.href)) return modules.get(url.href);
	const compiled = ts.transpileModule(readFileSync(url, 'utf8'), {
		compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
	}).outputText;
	const module = { exports: {} };
	const mockRequire = (name) => {
		if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }) };
		if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
		if (name === './Popup') return {
			Popup: ({ children }) => React.createElement('section', null, children),
			PopupButton: ({ children, ...props }) => React.createElement('button', props, children),
			PopupError: () => null,
		};
		if (name === './ScrollingText') return { ScrollingText: ({ children }) => children };
		if (name === './Artwork') return { Artwork: () => null };
		if (name.startsWith('.')) {
			const source = new URL(`${name}.ts`, url);
			return load(existsSync(source) ? source : new URL(`${name}.tsx`, url));
		}
		return require(name);
	};
	new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);
	modules.set(url.href, module.exports);
	return module.exports;
}

/** Expand function children while retaining the actual host properties and event handlers. */
function nodes(element) {
	if (Array.isArray(element)) return element.flatMap(nodes);
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Preserve provider strings literally, including Unicode and angle brackets. */
function text(element) {
	if (Array.isArray(element)) return element.map(text).join('');
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

/** Fixtures use the UTF-8 positions supplied by Rust, not JavaScript character indices. */
function range(body, value, from = 0) {
	const start = body.indexOf(value, from);
	assert.ok(start >= 0);
	const start_byte = new TextEncoder().encode(body.slice(0, start)).length;
	return { start_byte, end_byte: start_byte + new TextEncoder().encode(value).length };
}

const { Description } = load(new URL('./components/Description.tsx', import.meta.url));
const { Details } = load(new URL('./components/Details.tsx', import.meta.url));
const { VideoCommentsPopup } = load(new URL('./components/popups.tsx', import.meta.url));
const address = 'sales+music@example.org';
const body = `📮 Напишите (${address}), #музыка; raw@test.example <b>literal</b>`;
const link = (label, url, description_range = null, internal_target = null) => ({
	prefix: '', label, url, description_range, internal_target, presentation: 'LabelOnly', wikidata_item_id: null,
});
const links = [link('Website', 'https://example.org'),
	link(address, `mailto:${address}`, range(body, address)),
	link('#музыка', 'https://www.youtube.com/hashtag/music', range(body, '#музыка'), { YouTubeHashtag: 'music' })];
const description = (patch = {}) => Description({ text: body, links, timecodes: [], videoLinks: [], mediaId: null, ...patch });
const buttons = (tree) => nodes(tree).filter((node) => node.type === 'button');

test('description email keeps Unicode, punctuation and highlights while dispatching only its core index', () => {
	actions.length = 0;
	const tree = description({ highlights: [range(body, 'sales+music')] });
	assert.equal(text(tree), body);
	assert.equal(nodes(tree).filter((node) => node.type === 'mark').map(text).join(''), 'sales+music');
	assert.equal(nodes(tree).some((node) => node.type === 'b' || node.type === 'a'), false);
	assert.deepEqual(buttons(tree).map(text), [address, '#музыка']);
	const email = buttons(tree)[0];
	assert.equal(email.props.title, 'Compose email in your default mail app');
	assert.equal(email.props.disabled, false);
	assert.deepEqual(actions, [], 'rendering must not launch an email app');
	email.props.onFocus();
	assert.deepEqual(actions, [{ SelectDetailLink: 1 }], 'focusing only selects the indexed link');
	email.props.onClick();
	assert.deepEqual(actions, [{ SelectDetailLink: 1 }, { ActivateDetailLink: 1 }]);
});

test('unavailable external opener disables description email but preserves internal metadata actions', () => {
	const tree = description({ externalOpenerAvailable: false });
	const [email, hashtag] = buttons(tree);
	assert.equal(email.props.disabled, true);
	assert.equal(email.props.title, 'No external opener available');
	assert.equal(hashtag.props.disabled, false);
	actions.length = 0;
	hashtag.props.onClick();
	assert.deepEqual(actions, [{ ActivateDetailLink: 2 }]);
	assert.equal(buttons(description({ links: [] })).length, 0, 'unannotated email-looking text stays inert');
});

/** The rail is rendered by Details itself so its email branch cannot silently differ from inline links. */
const detailRail = (externalOpenerAvailable) => Details({ kind: 'Video', view: {
	screen: 'Search', playlist_item: null, details_focused: false, external_opener_available: externalOpenerAvailable,
	details: { title: 'Email fixture', source: 'YouTube', media_id: null, channel_id: '', channel_name: '',
		playlist_names: [], dearrow_title: null, description: '', timecodes: [], video_links: [], wikidata_entities: [],
		links: [link('Website', 'https://example.org'), link(address, `mailto:${address}`)] },
} });

for (const [label, render] of [
	['inline description', (available) => description({ externalOpenerAvailable: available })],
	['Details rail', detailRail],
]) {
	test(`${label} email Enter activates its index without the global selection key path`, () => {
		for (const available of [false, true]) {
			const tree = render(available);
			const email = buttons(tree).find((node) => text(node) === address);
			assert.equal(typeof email.props.onKeyDown, 'function', 'email needs explicit Enter handling before the document key bridge');
			actions.length = 0;
			const events = [];
			const event = (key) => ({ key, preventDefault: () => events.push('prevent'), stopPropagation: () => events.push('stop') });
			email.props.onKeyDown(event('Enter'));
			assert.deepEqual(events, ['prevent', 'stop']);
			assert.deepEqual(actions, available ? [{ ActivateDetailLink: 1 }] : [], 'only an available opener can compose');
			events.length = 0;
			actions.length = 0;
			email.props.onKeyDown(event('Escape'));
			const nonemail = buttons(tree).find((node) => ['#музыка', 'Website'].includes(text(node)));
			nonemail.props.onKeyDown?.(event('Enter'));
			assert.deepEqual(events, [], 'nonemail Enter and other keys retain their existing route');
			assert.deepEqual(actions, []);
		}
	});
}

const commentText = `📮 Привет (${address}), again ${address}. <script>literal</script>\n`;
const commentLinks = [range(commentText, address), range(commentText, address, commentText.indexOf(address) + address.length)]
	.map((span) => ({ ...span, url: `mailto:${address}` }));
const popup = (source) => ({ source, video_id: 'original-owner', video_title: 'Fixture', state: 'Ready', scroll_offset: 0,
	comments: [{ author_name: 'Reader', author_url: null, like_count: 2, published: null, text: commentText, email_links: commentLinks }] });

test('all public comment providers render core email spans and capture their original popup owner', () => {
	for (const source of ['you-tube', 'sound-cloud', 'archive-org']) {
		actions.length = 0;
		const original = popup(source);
		const tree = VideoCommentsPopup({ popup: original, externalOpenerAvailable: true });
		const emails = buttons(tree);
		assert.deepEqual(emails.map(text), [address, address], source);
		assert.ok(text(tree).endsWith(commentText.trimEnd()), 'comment text and punctuation remain intact');
		assert.equal(nodes(tree).some((node) => node.type === 'script' || node.type === 'a'), false);
		assert.deepEqual(actions, []);
		VideoCommentsPopup({ popup: { ...original, video_id: 'replacement-owner' }, externalOpenerAvailable: true });
		emails[1].props.onClick();
		assert.deepEqual(actions, [{ ActivateCommentEmail: { source, video_id: 'original-owner', comment_index: 0, email_index: 1 } }]);
	}
});

test('comment email Enter dispatches once and consumes the global popup key path', () => {
	const email = buttons(VideoCommentsPopup({ popup: popup('you-tube'), externalOpenerAvailable: true }))[0];
	assert.ok(email, 'core email span must be actionable');
	actions.length = 0;
	const events = [];
	email.props.onKeyDown({ key: 'Enter', preventDefault: () => events.push('prevent'), stopPropagation: () => events.push('stop') });
	assert.deepEqual(events, ['prevent', 'stop']);
	assert.equal(actions.length, 1);
	email.props.onKeyDown({ key: 'Escape', preventDefault: () => events.push('wrong'), stopPropagation: () => events.push('wrong') });
	assert.deepEqual(events, ['prevent', 'stop'], 'normal popup keys still propagate');
	assert.equal(actions.length, 1);
});

test('comments without an external opener remain readable and unannotated addresses stay inert', () => {
	const original = popup('you-tube');
	const tree = VideoCommentsPopup({ popup: original, externalOpenerAvailable: false });
	assert.deepEqual(buttons(tree).map((node) => node.props.disabled), [true, true]);
	assert.ok(buttons(tree).every((node) => node.props.title === 'No external opener available'));
	assert.ok(text(tree).endsWith(commentText.trimEnd()));
	original.comments[0].email_links = [];
	assert.deepEqual(buttons(VideoCommentsPopup({ popup: original, externalOpenerAvailable: true })), []);
});
