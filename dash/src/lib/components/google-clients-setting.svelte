<script lang="ts">
	import { onMount } from 'svelte';
	import KeyRound from '@lucide/svelte/icons/key-round';
	import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
	import Lock from '@lucide/svelte/icons/lock';
	import Radar from '@lucide/svelte/icons/radar';
	import ProviderMark from './provider-mark.svelte';
	import { management, providerName, type OAuthClientView } from '$lib/management';
	import { canEdit, canReset, clientInput, detectNeedsConfirm, draftProblem, initialDraft, replacesClient, secretRequired, signsInFromDashboard, type ClientDraft } from '$lib/google-clients';
	import { t } from '$lib/i18n.svelte';
	let { busy, updated }: { busy: string; updated: (message: string) => void | Promise<void> } = $props();
	let clients = $state.raw<OAuthClientView[]>([]);
	let loading = $state(true);
	let saving = $state(false);
	let detecting = $state('');
	let failure = $state('');
	let editing = $state('');
	let confirming = $state('');
	// A detection that would replace a client set by hand waits for this confirmation.
	let confirmingDetect = $state('');
	// The secret lives only in this draft while the form is open; it is cleared on save, cancel and unmount.
	let draft = $state<ClientDraft>({ client_id: '', client_secret: '', project_id: '' });
	const editedView = $derived(clients.find(client => client.provider === editing));

	async function load() {
		loading = true; failure = '';
		try { clients = (await management.oauthClients()).clients; }
		catch (error) { failure = (error as Error).message; }
		finally { loading = false; }
	}
	onMount(() => { void load(); return () => { draft.client_secret = ''; }; });

	function open(view: OAuthClientView) { failure = ''; confirming = ''; confirmingDetect = ''; editing = view.provider; draft = initialDraft(view); }
	function cancel() { editing = ''; draft = { client_id: '', client_secret: '', project_id: '' }; failure = ''; }
	function replace(view: OAuthClientView) { clients = clients.map(client => client.provider === view.provider ? view : client); }

	async function detect(view: OAuthClientView, force = false) {
		if (detecting || saving) return;
		failure = ''; confirming = ''; cancel();
		if (!force && detectNeedsConfirm(view)) { confirmingDetect = view.provider; return; }
		detecting = view.provider; confirmingDetect = '';
		try {
			replace(await management.detectAntigravityClient({ force }));
			await updated(t('notice.googleClientDetected', { provider: providerName(view.provider) }));
		} catch (error) { failure = (error as Error).message; }
		finally { detecting = ''; }
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		const view = editedView; if (!view || saving) return;
		const problem = draftProblem(view, draft);
		if (problem) { failure = t(`googleClients.invalid.${problem}`); return; }
		saving = true; failure = '';
		try {
			const input = clientInput(draft);
			replace(await management.saveOAuthClient(view.provider, input));
			editing = ''; draft = { client_id: '', client_secret: '', project_id: '' };
			await updated(t('notice.googleClientSaved', { provider: providerName(view.provider) }));
		} catch (error) { failure = (error as Error).message; }
		finally { saving = false; }
	}

	async function reset(view: OAuthClientView) {
		if (saving) return;
		saving = true; failure = '';
		try {
			replace(await management.resetOAuthClient(view.provider));
			confirming = ''; if (editing === view.provider) cancel();
			await updated(t('notice.googleClientReset', { provider: providerName(view.provider) }));
		} catch (error) { failure = (error as Error).message; }
		finally { saving = false; }
	}
</script>

<section class="google-clients" id="google-clients">
	<div><h2>{t('googleClients.title')}</h2><p>{t('googleClients.description')}</p></div>
	<div class="clients-body">
		{#if loading && !clients.length}<p class="muted small" role="status">{t('googleClients.loading')}</p>{/if}
		{#if clients.length}
			<div class="client-list">
				{#each clients as client (client.provider)}
					<div class="client-row">
						<div class="client-head">
							<span class="provider-label"><ProviderMark provider={client.provider} size={22}/>{providerName(client.provider)}</span>
							<span class="state-badge" class:inactive={client.source === 'none'} class:warning={client.source !== 'none' && !client.has_secret} title={t(client.has_secret ? 'googleClients.ready' : 'googleClients.notReady')}><span class="status-pip"></span>{t(`googleClients.source.${client.source}`)}</span>
						</div>
						{#if client.client_id}
							<dl class="client-facts"><dt>{t('googleClients.clientId')}</dt><dd><code class="inline-code" title={client.client_id}>{client.client_id}</code></dd>{#if client.project_id}<dt>{t('googleClients.projectId')}</dt><dd><code class="inline-code">{client.project_id}</code></dd>{/if}</dl>
						{:else}<p class="muted small">{t('googleClients.noClient')}</p>{/if}
						{#if client.origin === 'antigravity-cli'}<p class="muted small origin-note"><Radar size={14}/>{t('googleClients.origin.antigravity-cli')}</p>{/if}
						{#if !signsInFromDashboard(client.provider)}<p class="muted small">{t('googleClients.geminiUsage')}</p>{/if}
						{#if client.provider === 'antigravity' && canEdit(client) && !client.detectable && editing !== client.provider}<p class="muted small">{t('googleClients.detectUnavailable')}</p>{/if}
						{#if client.source === 'env'}
							<p class="env-note"><Lock size={15}/><span>{t('googleClients.envNote', { prefix: client.provider.toUpperCase() })}</span></p>
						{:else if editing === client.provider}
							<form class="client-form" onsubmit={save}>
								<label for={`${client.provider}-client-id`}>{t('googleClients.clientId')}</label>
								<input id={`${client.provider}-client-id`} bind:value={draft.client_id} required maxlength="512" autocomplete="off" spellcheck="false" placeholder="1234567890-abc.apps.googleusercontent.com"/>
								<label for={`${client.provider}-client-secret`}>{t('googleClients.clientSecret')}</label>
								<input id={`${client.provider}-client-secret`} type="password" bind:value={draft.client_secret} required={secretRequired(client, draft.client_id)} maxlength="512" autocomplete="new-password" spellcheck="false" placeholder={secretRequired(client, draft.client_id) ? '' : t('googleClients.secretKeep')}/>
								<label for={`${client.provider}-project-id`}>{t('googleClients.projectOptional')}</label>
								<input id={`${client.provider}-project-id`} bind:value={draft.project_id} maxlength="256" autocomplete="off" spellcheck="false"/>
								<p class="muted small">{t('googleClients.secretNote')}</p>
								{#if replacesClient(client, draft.client_id)}<p class="replace-note" role="note">{t('googleClients.replaceWarning')}</p>{/if}
								<div class="client-actions"><button class="button primary" type="submit" disabled={saving || !!busy}>{saving ? t('googleClients.saving') : t('googleClients.save')}</button><button class="button" type="button" disabled={saving} onclick={cancel}>{t('common.cancel')}</button></div>
							</form>
						{:else if confirming === client.provider}
							<div class="reset-confirmation" role="group" aria-label={t('googleClients.reset')}>
								<p>{t('googleClients.resetConfirm.none', { provider: providerName(client.provider) })}</p>
								<div class="client-actions"><button class="button danger" type="button" disabled={saving || !!busy} onclick={() => reset(client)}>{t('googleClients.reset')}</button><button class="button" type="button" disabled={saving} onclick={() => confirming = ''}>{t('common.cancel')}</button></div>
							</div>
						{:else if confirmingDetect === client.provider}
							<div class="reset-confirmation" role="group" aria-label={t('googleClients.detect')}>
								<p>{t('googleClients.replaceWarning')}</p>
								<div class="client-actions"><button class="button danger" type="button" disabled={!!detecting || saving || !!busy} onclick={() => detect(client, true)}><Radar size={16}/>{detecting === client.provider ? t('googleClients.detecting') : t('googleClients.detectReplace')}</button><button class="button" type="button" disabled={!!detecting} onclick={() => confirmingDetect = ''}>{t('common.cancel')}</button></div>
							</div>
						{:else if canEdit(client)}
							<div class="client-actions">
								{#if client.provider === 'antigravity' && client.detectable}<button class="button" type="button" disabled={!!detecting || saving || !!busy} title={t('googleClients.detectHint')} onclick={() => detect(client)}><Radar size={16}/>{detecting === client.provider ? t('googleClients.detecting') : t('googleClients.detect')}</button>{/if}
								<button class="button" type="button" disabled={!!detecting || saving || !!busy} onclick={() => open(client)}><KeyRound size={16}/>{t(client.source === 'file' ? 'googleClients.edit' : 'googleClients.setCustom')}</button>
								{#if canReset(client)}<button class="button subtle" type="button" disabled={!!detecting || saving || !!busy} onclick={() => { failure = ''; confirmingDetect = ''; confirming = client.provider; }}><RotateCcw size={16}/>{t('googleClients.reset')}</button>{/if}
							</div>
						{/if}
					</div>
				{/each}
			</div>
		{/if}
		{#if failure}<div class="clients-error" role="alert"><span>{failure}</span>{#if !clients.length}<button class="button small" type="button" disabled={loading} onclick={load}>{t('common.retry')}</button>{/if}</div>{/if}
	</div>
</section>

<style>
	.google-clients { display: grid; grid-template-columns: 245px 1fr; gap: 34px; max-width: 1060px; margin-top: 30px; padding: 30px 0 0; border-top: 1px solid var(--border); scroll-margin-top: 24px; }
	h2 { font-size: 16px; margin: 0 0 9px; }
	p { color: var(--muted); font-size: 15px; line-height: 1.65; margin: 0; }
	.clients-body { display: grid; gap: 16px; align-content: start; min-width: 0; }
	.client-list { border: 1px solid var(--border); border-radius: 12px; background: var(--surface); overflow: hidden; }
	.client-row { display: grid; gap: 12px; padding: 16px; border-bottom: 1px solid var(--border); min-width: 0; }
	.client-row:last-child { border-bottom: 0; }
	.client-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
	.provider-label { display: flex; align-items: center; gap: 11px; font-size: 15px; }
	.client-facts { display: grid; grid-template-columns: auto minmax(0,1fr); gap: 8px 14px; align-items: baseline; margin: 0; font-size: 15px; }
	.client-facts dt { color: var(--muted); }
	.client-facts dd { margin: 0; min-width: 0; }
	.client-facts code { display: inline-block; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; vertical-align: bottom; overflow-wrap: normal; }
	.env-note { display: flex; align-items: flex-start; gap: 10px; }
	.env-note :global(svg) { flex-shrink: 0; color: var(--faint); margin-top: 4px; }
	.origin-note { display: flex; align-items: center; gap: 7px; }
	.origin-note :global(svg) { flex-shrink: 0; color: var(--faint); }
	.client-form { display: grid; gap: 10px; padding-top: 4px; }
	.client-form label { font-size: 15px; }
	.client-form input { width: 100%; font-size: 15px; }
	.client-actions { display: flex; flex-wrap: wrap; justify-content: flex-end; gap: 10px; }
	.reset-confirmation { display: grid; gap: 14px; }
	.reset-confirmation p, .replace-note { color: var(--text); }
	.clients-error { display: flex; align-items: center; justify-content: space-between; gap: 12px; color: var(--red); font-size: 15px; line-height: 1.6; }
	@media(max-width:1150px) { .google-clients { grid-template-columns: 1fr; gap: 22px; } }
	@media(max-width:650px) { .client-row { padding: 14px 12px; }.client-facts { grid-template-columns: 1fr; gap: 4px; }.client-facts dd + dt { margin-top: 6px; }.client-actions .button { flex: 1; } }
</style>
