import { test } from 'node:test';
import assert from 'node:assert/strict';
import { connectMode, canEdit, canReset, initialDraft, secretRequired, draftProblem, clientInput, replacesClient, callbackHintKey, signsInFromDashboard } from '../src/lib/google-clients.ts';
import { en } from '../src/lib/strings/en.ts';
import { fr } from '../src/lib/strings/fr.ts';

const view = (source, extra = {}) => ({ provider: 'antigravity', source, client_id: source === 'none' ? null : 'id.apps.googleusercontent.com', has_secret: source !== 'none', project_id: null, origin: null, read_only: source === 'env', ...extra });
const draft = (client_id, client_secret = '', project_id = '') => ({ client_id, client_secret, project_id });

test('Antigravity uses its OAuth flow when offered and never the token form otherwise', () => {
	assert.equal(connectMode({ id: 'antigravity', oauth: true }), 'oauth');
	assert.equal(connectMode({ id: 'antigravity', oauth: false }), 'google-setup');
	assert.equal(connectMode({ id: 'kimi', oauth: false }), 'token');
	assert.equal(connectMode({ id: 'openai_compatible', oauth: false }), 'endpoint');
	assert.equal(connectMode({ id: 'claude', oauth: true }), 'oauth');
	// Gemini is never offered in Connect: its client serves douanectl and the API.
	assert.ok(signsInFromDashboard('antigravity'));
	assert.ok(!signsInFromDashboard('gemini'));
});

test('environment clients are read-only; only a file entry resets', () => {
	assert.ok(!canEdit(view('env')));
	assert.ok(!canReset(view('env')));
	assert.ok(canEdit(view('none')) && !canReset(view('none')));
	assert.ok(canEdit(view('file')) && canReset(view('file')));
});

test('the form never prefills a secret, and an unconfigured client starts empty', () => {
	assert.deepEqual(initialDraft(view('none')), draft(''));
	assert.deepEqual(initialDraft(view('file', { project_id: 'my-project' })), draft('id.apps.googleusercontent.com', '', 'my-project'));
	assert.equal(initialDraft(view('file')).client_secret, '');
});

test('the secret may stay empty only for the same custom client id with a saved secret', () => {
	assert.ok(!secretRequired(view('file'), ' id.apps.googleusercontent.com '));
	assert.ok(secretRequired(view('file'), 'other.apps.googleusercontent.com'));
	assert.ok(secretRequired(view('file', { has_secret: false }), 'id.apps.googleusercontent.com'));
	assert.ok(secretRequired(view('none'), 'id'));
});

test('drafts are checked like the server checks them', () => {
	assert.equal(draftProblem(view('none'), draft('  ')), 'client_id');
	assert.equal(draftProblem(view('none'), draft('a\nb', 's')), 'client_id');
	assert.equal(draftProblem(view('none'), draft('x'.repeat(513), 's')), 'client_id');
	assert.equal(draftProblem(view('none'), draft('id', '')), 'client_secret');
	assert.equal(draftProblem(view('none'), draft('id', 'é'.repeat(257))), 'client_secret');
	assert.equal(draftProblem(view('none'), draft('id', 's', 'p\nq')), 'project_id');
	assert.equal(draftProblem(view('none'), draft('id', 's', 'p'.repeat(257))), 'project_id');
	assert.equal(draftProblem(view('none'), draft('id', 's')), null);
	assert.equal(draftProblem(view('file'), draft('id.apps.googleusercontent.com')), null);
	// Any client id, including the public Antigravity id, is accepted as a custom client.
	assert.equal(draftProblem(view('none'), draft('1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com', 's')), null);
});

test('every control character is refused, as the server refuses it', () => {
	for (const control of ['\u0000', '\t', '\u000b', '\u001f', '\u007f', '\u0085', '\u009f']) {
		assert.equal(draftProblem(view('none'), draft(`a${control}b`, 's')), 'client_id', JSON.stringify(control));
		assert.equal(draftProblem(view('none'), draft('id', `s${control}s`)), 'client_secret', JSON.stringify(control));
		assert.equal(draftProblem(view('none'), draft('id', 's', `p${control}q`)), 'project_id', JSON.stringify(control));
	}
	// Surrounding whitespace is trimmed first, as the server trims it.
	assert.equal(draftProblem(view('none'), draft(' id\n', '\ts ', 'p\r\n')), null);
	assert.equal(draftProblem(view('none'), draft('id x', 's')), null);
});

test('replacing a client warns that its accounts must reconnect', () => {
	assert.ok(replacesClient(view('file'), 'other.apps.googleusercontent.com'));
	assert.ok(!replacesClient(view('file'), ' id.apps.googleusercontent.com '));
	assert.ok(!replacesClient(view('env'), 'other.apps.googleusercontent.com'));
	assert.ok(!replacesClient(view('none'), 'other'));
});

test('Google texts fit the provider, in every language', () => {
	assert.equal(callbackHintKey('antigravity'), 'connect.callbackHint.antigravity');
	assert.equal(callbackHintKey('claude'), 'connect.callbackHint');
	for (const strings of [en, fr]) {
		assert.match(strings['connect.googleSetupImportText'], /Antigravity/);
		assert.match(strings[callbackHintKey('claude')], /Claude/);
		assert.doesNotMatch(strings[callbackHintKey('antigravity')], /Claude/);
		// Gemini is used through douanectl, the API and import, never a dashboard sign-in.
		for (const key of ['googleClients.description', 'googleClients.geminiUsage']) assert.match(strings[key], /douanectl/);
		assert.ok(!Object.keys(strings).some((key) => key.endsWith('.gemini')));
		// The missing-project text names no single provider.
		assert.match(strings['api.googleProjectUnavailable'], /Antigravity/);
		assert.match(strings['api.googleProjectUnavailable'], /Gemini/);
		for (const key of ['googleClients.resetConfirm.none', 'googleClients.replaceWarning']) assert.match(strings[key], /reconnect/);
		// The setup guide and detect strings exist in both languages.
		for (const key of ['connect.googleSetupIntro', 'connect.googleSetupDetect', 'connect.googleSetupRemoteText', 'googleClients.detect', 'googleClients.origin.antigravity-cli', 'notice.googleClientDetected']) assert.ok(strings[key], key);
	}
});

test('no built-in source or secret is referenced any more', () => {
	for (const strings of [en, fr]) {
		assert.ok(!('googleClients.source.builtin' in strings));
		assert.ok(!('googleClients.invalid.builtin' in strings));
		assert.ok(!('googleClients.resetConfirm.builtin' in strings));
	}
});

test('the request trims values, omits an empty secret and clears an empty project', () => {
	assert.deepEqual(clientInput(draft(' id ', ' s ', ' p ')), { client_id: 'id', client_secret: 's', project_id: 'p' });
	assert.deepEqual(clientInput(draft('id', '  ', '')), { client_id: 'id', project_id: null });
});
