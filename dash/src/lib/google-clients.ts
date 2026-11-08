// Pure rules for the Google sign-in clients (Antigravity, Gemini) and the
// connect dialog. No runtime import of the i18n store, so node tests can load it.
import type { OAuthClientInput, OAuthClientView, Provider } from './management';

/**
 * Whether the dashboard signs in to a provider whose Google client is set in
 * Settings. Only Antigravity is offered in Connect; Gemini's client serves
 * `douanectl login gemini` and the management API, and Gemini accounts come
 * from those or from an imported credential file.
 */
export function signsInFromDashboard(provider: OAuthClientView['provider']) { return provider === 'antigravity'; }

/**
 * What the connect dialog offers for a provider: its OAuth flow, the setup
 * explanation for Antigravity without a usable client (never a token form:
 * a bare access token expires within the hour), or a manual form.
 */
export type ConnectMode = 'oauth' | 'google-setup' | 'endpoint' | 'token';
export function connectMode(provider: Pick<Provider, 'id' | 'oauth'>): ConnectMode {
	if (provider.oauth) return 'oauth';
	if (provider.id === 'antigravity') return 'google-setup';
	return provider.id === 'openai_compatible' ? 'endpoint' : 'token';
}

/** The form fields, as typed; the secret is never prefilled. */
export type ClientDraft = { client_id: string; client_secret: string; project_id: string };

/** A custom entry from `oauth-clients.json` can be reset; environment variables cannot be changed here. */
export function canReset(view: OAuthClientView) { return view.source === 'file' && !view.read_only; }
export function canEdit(view: OAuthClientView) { return !view.read_only && view.source !== 'env'; }

/** Starting values: only a custom entry prefills its id and project. */
export function initialDraft(view: OAuthClientView): ClientDraft {
	return view.source === 'file' ? { client_id: view.client_id ?? '', client_secret: '', project_id: view.project_id ?? '' } : { client_id: '', client_secret: '', project_id: '' };
}

/** The server keeps a stored secret only for the same custom client id, so the field may stay empty just then. */
export function secretRequired(view: OAuthClientView, clientId: string) {
	return !(view.source === 'file' && view.has_secret && !!view.client_id && clientId.trim() === view.client_id);
}

/** Non-empty after trim, no control character (Rust's `char::is_control`), within `max` UTF-8 bytes: the server's check. */
function valid(value: string, max: number) { const trimmed = value.trim(); return !!trimmed && !/[\u0000-\u001f\u007f-\u009f]/.test(trimmed) && new TextEncoder().encode(trimmed).length <= max; }

export type DraftProblem = 'client_id' | 'client_secret' | 'project_id';
/** The first field the server would refuse, or null. */
export function draftProblem(view: OAuthClientView, draft: ClientDraft): DraftProblem | null {
	if (!valid(draft.client_id, 512)) return 'client_id';
	if (draft.client_secret.trim() || secretRequired(view, draft.client_id)) { if (!valid(draft.client_secret, 512)) return 'client_secret'; }
	if (draft.project_id.trim() && !valid(draft.project_id, 256)) return 'project_id';
	return null;
}

/**
 * Saving another client id replaces the custom client: accounts signed in with
 * it can no longer refresh and must reconnect. Built-in client accounts keep working.
 */
export function replacesClient(view: OAuthClientView, clientId: string) {
	return view.source === 'file' && !!view.client_id && clientId.trim() !== view.client_id;
}

/** Antigravity's public client id (not a secret): a detected client always uses it. */
export const ANTIGRAVITY_CLIENT_ID = '1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com';

/**
 * Detecting replaces a client chosen by hand (another id, not detected by
 * douane), whose accounts must then reconnect: confirm first, then ask the
 * server with `force`, which refuses otherwise. Replacing a detected client,
 * or detecting with none set, needs no confirmation.
 */
export function detectNeedsConfirm(view: OAuthClientView | null | undefined) {
	return !!view && view.source === 'file' && !!view.client_id && view.client_id !== ANTIGRAVITY_CLIENT_ID && view.origin !== 'antigravity-cli';
}

/**
 * The douane address douanectl must use: this page's origin plus any reverse
 * proxy prefix in front of the dashboard's base path (`/admin`), so
 * `https://host/douane/admin/` gives `https://host/douane`.
 */
export function serverUrl(origin: string, pathname: string, base: string) {
	if (!base) return origin;
	const inner = pathname.indexOf(`${base}/`);
	const at = inner >= 0 ? inner : pathname.endsWith(base) ? pathname.length - base.length : 0;
	return `${origin}${pathname.slice(0, at).replace(/\/+$/, '')}`;
}

/** The douanectl command that detects the client on another machine and sends it to `server`. */
export function detectCommand(server: string) { return `douanectl --url ${server} oauth-client detect-antigravity`; }

/** The hint beside the callback field; the Claude code hint is never shown for Antigravity. */
export function callbackHintKey(provider: string) {
	return provider === 'antigravity' ? 'connect.callbackHint.antigravity' : 'connect.callbackHint';
}

/** The PUT body: trimmed values, an empty project removes it, an empty secret is omitted to keep the stored one. */
export function clientInput(draft: ClientDraft): OAuthClientInput {
	const secret = draft.client_secret.trim(), project = draft.project_id.trim();
	return { client_id: draft.client_id.trim(), ...(secret ? { client_secret: secret } : {}), project_id: project || null };
}
