/**
 * Opt-in, real-browser integration check for the built frontend and mocked IPC.
 * Run after `npm --prefix gui/ui run build`:
 *   npm --prefix gui/ui run test:browser
 *
 * Uses a fresh Firefox profile, loopback-only HTTP server and no real Tauri
 * bridge, player, provider or upload credentials. Rust reducer/playback and
 * native WebKit behavior require separate validation.
 */
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

const require = createRequire(import.meta.url);
const ts = require('typescript');
const firefox = process.env.YOUTA_TEST_FIREFOX ?? 'firefox';
const available = spawnSync(firefox, ['--version'], { timeout: 5000 }).status === 0;

/** Derive neutral fixture defaults from the public contract, not a second DTO. */
async function contractDefaults() {
	const source = await readFile(new URL('../src/contract.ts', import.meta.url), 'utf8');
	const parsed = ts.createSourceFile('contract.ts', source, ts.ScriptTarget.Latest, true);
	const types = new Map(parsed.statements.filter((node) =>
		ts.isInterfaceDeclaration(node) || ts.isTypeAliasDeclaration(node),
	).map((node) => [node.name.text, node]));
	const value = (node) => {
		if (node.kind === ts.SyntaxKind.StringKeyword) return '';
		if (node.kind === ts.SyntaxKind.NumberKeyword) return 0;
		if (node.kind === ts.SyntaxKind.BooleanKeyword) return false;
		if (ts.isArrayTypeNode(node)) return [];
		if (ts.isUnionTypeNode(node)) return node.types.some((part) =>
			ts.isLiteralTypeNode(part) && part.literal.kind === ts.SyntaxKind.NullKeyword,
		) ? null : value(node.types[0]);
		if (ts.isLiteralTypeNode(node)) return ts.isStringLiteral(node.literal) ? node.literal.text : null;
		if (ts.isTypeReferenceNode(node)) return named(node.typeName.getText(parsed));
		throw new Error(`Unsupported fixture contract type: ${node.getText(parsed)}`);
	};
	const named = (name) => {
		const node = types.get(name);
		assert.ok(node, `Unknown fixture contract: ${name}`);
		if (ts.isTypeAliasDeclaration(node)) return value(node.type);
		return Object.fromEntries(node.members.map((member) => [member.name.getText(parsed), value(member.type)]));
	};
	return Object.fromEntries(['ViewModel', 'DetailView', 'RowView', 'ArchiveUploadPopupView', 'S3UploadPopupView']
		.map((name) => [name, named(name)]));
}

test('Firefox renders Archive browsing, EOF seek controls and upload dialogs through mocked IPC', {
	skip: !available && 'Firefox is unavailable; set YOUTA_TEST_FIREFOX to its executable',
	timeout: 70000,
}, async (context) => {
	const defaults = await contractDefaults();
	const frontend = new URL('../../frontend/', import.meta.url);
	const index = (await readFile(new URL('index.html', frontend), 'utf8'))
		.replace('</head>', '<script src="/__fixture.js"></script></head>');
	const fixture = `window.__YOUTA_FIXTURES__ = ${JSON.stringify(defaults)};\n`
		+ await readFile(new URL('./browser.fixture.js', import.meta.url), 'utf8');
	const assets = new Map([
		['/', ['text/html', index]],
		['/app.js', ['text/javascript', await readFile(new URL('app.js', frontend))]],
		['/app.css', ['text/css', await readFile(new URL('app.css', frontend))]],
		['/__fixture.js', ['text/javascript', fixture]],
	]);
	let resolveReport;
	const report = new Promise((resolve) => { resolveReport = resolve; });
	const server = createServer(async (request, response) => {
		if (request.method === 'POST' && request.url === '/__report') {
			let body = '';
			for await (const chunk of request) {
				body += chunk;
				if (body.length > 64 * 1024) { request.destroy(); return; }
			}
			try { resolveReport(JSON.parse(body)); } catch { resolveReport({ ok: false, error: 'Invalid browser report' }); }
			response.writeHead(204).end();
			return;
		}
		const asset = assets.get(request.url);
		if (!asset || request.method !== 'GET') { response.writeHead(404).end(); return; }
		response.writeHead(200, {
			'Content-Type': asset[0],
			'Content-Security-Policy': "default-src 'self'; img-src 'self' data:; connect-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'",
			'Cache-Control': 'no-store',
		});
		response.end(asset[1]);
	});
	await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
	const directory = await mkdtemp(join(tmpdir(), 'youta-browser-test-'));
	// Block browser background traffic as well as fixture fetches; loopback is exempt.
	await writeFile(join(directory, 'user.js'), [
		'user_pref("browser.shell.checkDefaultBrowser", false);',
		'user_pref("browser.startup.homepage_override.mstone", "ignore");',
		'user_pref("browser.newtabpage.enabled", false);',
		'user_pref("network.captive-portal-service.enabled", false);',
		'user_pref("network.connectivity-service.enabled", false);',
		'user_pref("network.proxy.type", 1);',
		'user_pref("network.proxy.http", "127.0.0.1");',
		'user_pref("network.proxy.http_port", 9);',
		'user_pref("network.proxy.ssl", "127.0.0.1");',
		'user_pref("network.proxy.ssl_port", 9);',
		'user_pref("network.proxy.no_proxies_on", "localhost,127.0.0.1");',
	].join('\n'));
	let diagnostics = '';
	const browser = spawn(firefox, [
		'--headless', '--no-remote', '--new-instance', '--profile', directory,
		'--width', '1280', '--height', '960', `http://127.0.0.1:${server.address().port}/`,
	], { detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
	for (const stream of [browser.stdout, browser.stderr]) stream.on('data', (chunk) => {
		diagnostics = (diagnostics + chunk).slice(-16 * 1024);
	});
	let timer;
	try {
		const result = await Promise.race([
			report,
			new Promise((resolve) => { timer = setTimeout(() => resolve({ ok: false, error: 'Browser report timed out' }), 55000); }),
			new Promise((resolve) => browser.once('error', (error) => resolve({ ok: false, error: error.message }))),
		]);
		assert.equal(result.ok, true, `${JSON.stringify(result, null, 2)}\n${diagnostics}`);
		assert.ok(result.checks.length >= 40, 'browser must complete all interaction checks');
		context.diagnostic(`${result.checks.length} browser interaction assertions passed; native playback and uploads were mocked`);
	} finally {
		clearTimeout(timer);
		// Only the process group created above and our private profile are cleaned up.
		if (browser.pid) { try { process.kill(-browser.pid, 'SIGTERM'); } catch {} }
		await new Promise((resolve) => {
			server.close(resolve);
			server.closeAllConnections();
		});
		if (browser.exitCode === null) {
			await Promise.race([new Promise((resolve) => browser.once('exit', resolve)), new Promise((resolve) => setTimeout(resolve, 2000))]);
		}
		if (browser.exitCode === null && browser.pid) { try { process.kill(-browser.pid, 'SIGKILL'); } catch {} }
		await rm(directory, { recursive: true, force: true });
	}
});
