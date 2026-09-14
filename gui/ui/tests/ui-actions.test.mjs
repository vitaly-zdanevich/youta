/** Semantic TypeScript checks for the existing externally tagged Rust action wire format. */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import ts from 'typescript';

const virtualPath = fileURLToPath(new URL('./__ui_action_type_fixture__.ts', import.meta.url));
const fixtures = JSON.parse(readFileSync(new URL('../../../tests/fixtures/ui-actions.json', import.meta.url), 'utf8'));
const options = {
	strict: true,
	noEmit: true,
	exactOptionalPropertyTypes: true,
	skipLibCheck: true,
	target: ts.ScriptTarget.ES2022,
	module: ts.ModuleKind.ESNext,
	moduleResolution: ts.ModuleResolutionKind.Bundler,
	types: [],
};

/** Type-checks an in-memory fixture without writing files or emitting JavaScript. */
function compile(source) {
	const host = ts.createCompilerHost(options);
	const readSource = host.getSourceFile.bind(host);
	host.getSourceFile = (path, languageVersion, onError, shouldCreateNewSourceFile) => path === virtualPath
		? ts.createSourceFile(path, source, languageVersion, true)
		: readSource(path, languageVersion, onError, shouldCreateNewSourceFile);
	const bridgeTypes = fileURLToPath(new URL('../src/tauri.d.ts', import.meta.url));
	const program = ts.createProgram([virtualPath, bridgeTypes], options, host);
	return { program, diagnostics: ts.getPreEmitDiagnostics(program) };
}

/** Reports semantic diagnostics with source locations when a contract drifts. */
function expectNoDiagnostics(diagnostics) {
	assert.equal(diagnostics.length, 0, ts.formatDiagnosticsWithColorAndContext(diagnostics, {
		getCanonicalFileName: (path) => path,
		getCurrentDirectory: () => process.cwd(),
		getNewLine: () => '\n',
	}));
}

/** Requires examples of every finite alternative and every nested payload field. */
function expectTypeCoverage(checker, type, values, path) {
	if (type.isUnion()) {
		for (const member of type.types) expectTypeCoverage(checker, member, values, path);
	} else if (type.isStringLiteral()) {
		assert.ok(values.includes(type.value), `${path} lacks ${type.value}`);
	} else if (type.flags & ts.TypeFlags.BooleanLiteral) {
		assert.ok(values.includes(type.intrinsicName === 'true'), `${path} lacks ${type.intrinsicName}`);
	} else if (type.flags & ts.TypeFlags.Null) {
		assert.ok(values.includes(null), `${path} lacks null`);
	} else if (type.flags & ts.TypeFlags.String) {
		assert.ok(values.some((value) => typeof value === 'string'), `${path} lacks a string`);
	} else if (type.flags & ts.TypeFlags.Number) {
		assert.ok(values.some((value) => typeof value === 'number'), `${path} lacks a number`);
	} else {
		const fields = type.getProperties();
		assert.ok(fields.length, `Unsupported fixture type ${path}: ${checker.typeToString(type)}`);
		for (const field of fields) {
			const samples = values.filter((value) => value && typeof value === 'object' && field.name in value)
				.map((value) => value[field.name]);
			expectTypeCoverage(checker, checker.getTypeOfSymbolAtLocation(field, field.valueDeclaration), samples, `${path}.${field.name}`);
		}
	}
}

test('shared Rust fixtures type-check and cover every frontend action and enum alternative', () => {
	const { program, diagnostics } = compile(`import type { UiAction } from '../src/contract';
const fixtures = ${JSON.stringify(fixtures)} satisfies readonly UiAction[];`);
	expectNoDiagnostics(diagnostics);
	const checker = program.getTypeChecker();
	const source = program.getSourceFile(fileURLToPath(new URL('../src/actions.ts', import.meta.url)));
	const exports = checker.getExportsOfModule(checker.getSymbolAtLocation(source));
	const declared = (name) => checker.getDeclaredTypeOfSymbol(exports.find((symbol) => symbol.name === name));
	const units = declared('UnitUiAction');
	expectTypeCoverage(checker, units, fixtures.filter((value) => typeof value === 'string'), 'UnitUiAction');
	const payloads = declared('UiActionPayloads');
	assert.deepEqual(new Set(fixtures.filter((value) => typeof value === 'object').flatMap(Object.keys)),
		new Set(payloads.getProperties().map((field) => field.name)));
	for (const field of payloads.getProperties()) {
		const values = fixtures.filter((value) => typeof value === 'object' && field.name in value).map((value) => value[field.name]);
		expectTypeCoverage(checker, checker.getTypeOfSymbolAtLocation(field, field.valueDeclaration), values, field.name);
	}
});

test('UiAction rejects unknown names, incorrect payloads, null identities and multiple tags', () => {
	const invalid = [
		'\'SeekAbsoluteSeconds\'',
		'\'SelectRow\'',
		'{ TogglePause: null }',
		'{ SelectRow: \'one\' }',
		'{ SelectRow: 1, SeekPercent: 25 }',
		'{ SetDetailsFocus: 1 }',
		'{ ShowScreen: \'NotAScreen\' }',
		'{ SetSubscriptionsLayout: \'stacked\' }',
		'{ SelectArchiveUploadField: \'SecretKey\' }',
		'{ SelectDownloadChoice: { generation: 1 } }',
		'{ SelectDownloadChoice: { generation: null, index: 0 } }',
		'{ ActivateTimecode: { media_id: null, seconds: 1 } }',
		'{ ActivateTimecode: { media_id: { source: \'local\' }, seconds: 1 } }',
		'{ ActivateTimecode: { media_id: { source: \'invented-provider\', external_id: \'id\' }, seconds: 1 } }',
		'{ ActivateTimecode: { media_id: { source: \'you-tube\', external_id: \'id\' } } }',
		'{ ActivateWaveformTimecode: { media_id: { source: \'local\', external_id: \'id\' }, seconds: 1 } }',
		'{ ActivateDescriptionVideo: { video_id: \'id\', start_seconds: \'1\' } }',
		'{ SelectDownloadChoice: { generation: 1, index: 0, invented: true } }',
	];
	const { diagnostics } = compile(`import { dispatch as send } from '../src/ipc';
${invalid.map((expression) => `// @ts-expect-error Invalid action must remain rejected.\nsend(${expression});`).join('\n')}
const twoPayloadTags = { SelectRow: 1, SeekPercent: 25 };
// @ts-expect-error Variables also cannot contain two known action tags.
send(twoPayloadTags);
const mixedActionTags = { SelectRow: 1, TogglePause: true };
// @ts-expect-error A second unit action tag cannot accompany a payload tag.
send(mixedActionTags);
const widenedName: string = 'TogglePause';
// @ts-expect-error Dynamic action tables must retain their exact action names.
send(widenedName);`);
	expectNoDiagnostics(diagnostics);
});
