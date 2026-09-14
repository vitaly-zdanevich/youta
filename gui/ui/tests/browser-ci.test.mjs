/** Regression coverage for required CI browser checks without launching Firefox. */
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

/** Use an absent repository-relative executable, independently of the host PATH. */
function withoutFirefox(ci) {
	const environment = { ...process.env };
	// A subprocess is a separate test run, not a nested invocation of this runner.
	delete environment.NODE_TEST_CONTEXT;
	return spawnSync(process.execPath, ['--test', '--test-reporter=tap', fileURLToPath(new URL('./browser.test.mjs', import.meta.url))], {
		encoding: 'utf8',
		timeout: 10000,
		env: {
			...environment,
			CI: ci,
			YOUTA_TEST_FIREFOX: fileURLToPath(new URL('./missing-firefox-executable', import.meta.url)),
		},
	});
}

test('missing Firefox fails the browser check in CI instead of skipping', () => {
	const result = withoutFirefox('true');
	assert.ifError(result.error);
	assert.equal(result.status, 1, result.stdout + result.stderr);
	assert.match(result.stdout + result.stderr, /Firefox is required in CI/);
	assert.doesNotMatch(result.stdout, /# SKIP/);
});

test('missing Firefox still permits an optional local browser check', () => {
	const result = withoutFirefox('false');
	assert.ifError(result.error);
	assert.equal(result.status, 0, result.stdout + result.stderr);
	assert.match(result.stdout, /# SKIP Firefox is unavailable/);
});

test('Linux desktop CI requires Firefox and tests the built page', async () => {
	const workflow = await readFile(new URL('../../../.github/workflows/ci.yml', import.meta.url), 'utf8');
	const desktop = workflow.split('\n  desktop:\n')[1]?.split('\n  desktop-linux-i686:\n')[0];
	assert.ok(desktop, 'desktop CI job exists');
	const steps = desktop.split('      - name: ');
	const browser = steps.find((step) => step.includes('npm --prefix gui/ui run test:browser'));
	assert.ok(browser, 'desktop CI must run the existing browser test');
	assert.match(browser, /if: runner\.os == 'Linux'/);
	assert.match(browser, /firefox --version/);
	assert.doesNotMatch(browser, /continue-on-error|\|\|\s*true/);
	assert.ok(desktop.indexOf('npm --prefix gui/ui run build') < desktop.indexOf(browser));
	assert.match(desktop, /timeout-minutes: 360/);
});
