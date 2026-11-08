<script lang="ts">
	import { onMount } from 'svelte';
	import { base } from '$app/paths';
	import X from '@lucide/svelte/icons/x';
	import ExternalLink from '@lucide/svelte/icons/external-link';
	import Upload from '@lucide/svelte/icons/upload';
	import Check from '@lucide/svelte/icons/check';
	import Copy from '@lucide/svelte/icons/copy';
	import ChevronRight from '@lucide/svelte/icons/chevron-right';
	import Settings2 from '@lucide/svelte/icons/settings-2';
	import Radar from '@lucide/svelte/icons/radar';
	import ProviderMark from './provider-mark.svelte';
	import { copyText } from '$lib/clipboard';
	import { t } from '$lib/i18n.svelte';
	import { callbackHintKey, connectMode, detectCommand as buildDetectCommand, detectNeedsConfirm, serverUrl } from '$lib/google-clients';
	import { management, importErrors, providerName, dialects, dialectLabel, type Dialect, type Provider, type OAuthClientView, type OAuthSession, type OAuthStatus } from '$lib/management';
	let { providers, close, completed, importFiles, openGoogleClients }: { providers: Provider[]; close: () => void; completed: () => void; importFiles: (files: FileList) => void; openGoogleClients: () => void } = $props();
	let dialog: HTMLDialogElement;
	let fileInput = $state<HTMLInputElement>();
	let session = $state.raw<OAuthSession | null>(null);
	let callback = $state('');
	let pending = $state(false);
	let failure = $state('');
	let success = $state(false);
	let terminal = $state(false);
	let status = $state('');
	let manual = $state('');
	// A Google provider without a usable OAuth client: a guided setup, never the token form.
	let setup = $state<'antigravity' | ''>('');
	let detecting = $state(false);
	// The Antigravity client as the server shows it: whether a CLI is installed there, and whether detecting would replace a client set by hand.
	let setupClient = $state.raw<OAuthClientView | null>(null);
	let confirmDetect = $state(false);
	let cmdCopied = $state(false);
	let detectCommand = $state('');
	let accountName = $state('');
	let credential = $state('');
	let endpoint = $state('');
	let route = $state('');
	let dialect = $state<Dialect | ''>('chat');
	const custom = $derived(manual === 'openai_compatible');
	let codeCopied = $state(false);
	let mounted = false;
	const endpoints: Record<string,string> = { opencode_go:'https://opencode.ai/zen/go',xai:'https://cli-chat-proxy.grok.com/v1',openai_compatible:'',kimi:'https://api.kimi.com/coding/v1',qwen:'https://portal.qwen.ai/v1' };
	onMount(() => { mounted = true; detectCommand = buildDetectCommand(serverUrl(location.origin, location.pathname, base)); dialog.showModal(); return () => { mounted = false; dialog.close(); credential = ''; callback = ''; }; });
	async function start(provider: string) {
		pending = true; failure = ''; callback = ''; status = ''; terminal = false; codeCopied = false;
		try { const attempt = await management.startOAuth(provider); if (mounted) { session = attempt; status = attempt.user_code ? t('connect.waiting') : ''; } }
		catch (error) { if (mounted) failure = (error as Error).message; }
		finally { pending = false; }
	}
	function connected() { success = true; terminal = true; callback = ''; credential = ''; completed(); }
	function requiredAction(attempt: OAuthSession, result: OAuthStatus) {
		if (!mounted || session?.id !== attempt.id) return false;
		if (!result.verification_required && !(attempt.provider === 'antigravity' && result.error_code === 'google_project_unavailable')) return false;
		session = { ...attempt, error_code: result.error_code, verification_required: result.verification_required, verification_available: result.verification_available };
		terminal = true; callback = ''; failure = ''; status = '';
		return true;
	}
	async function check(manualCheck = true) {
		const attempt = session; if (!attempt || pending || terminal) return attempt?.interval ?? 5;
		pending = true; if (manualCheck) failure = '';
		try {
			const result = await management.oauth(attempt.id);
			if (!mounted || session?.id !== attempt.id) return 5;
			if (requiredAction(attempt, result)) return 5;
			if ((result.account_id || result.account?.id) && ['complete','completed','connected','success'].includes(result.status)) connected();
			else if (result.status === 'expired') { terminal = true; failure = t('connect.expired'); }
			else if (result.error || result.status === 'error') { terminal = true; failure = result.error || t('connect.refused'); }
			else status = t(result.status === 'exchanging' ? 'connect.exchanging' : attempt.user_code ? 'connect.waiting' : 'connect.stillWaiting');
			return Math.min(60,Math.max(5,result.interval ?? attempt.interval ?? 5));
		} catch (error) { if (mounted) failure = (error as Error).message; return attempt.interval ?? 5; }
		finally { pending = false; }
	}
	$effect(() => {
		const attempt = session; if (!attempt?.user_code || success || terminal) return;
		let stopped = false;
		let timer: ReturnType<typeof setTimeout>;
		async function poll() { const interval = document.hidden ? attempt?.interval ?? 5 : await check(false); if (!stopped) timer = setTimeout(poll,(interval ?? 5)*1000); }
		timer = setTimeout(poll,Math.max(5,attempt.interval ?? 5)*1000);
		return () => { stopped = true; clearTimeout(timer); };
	});
	async function finish(event: SubmitEvent) {
		event.preventDefault(); const attempt = session; if (!attempt || pending) return;
		pending = true; failure = '';
		try { const result = await management.oauthCallback(attempt.id,callback.trim(),attempt.state); if (!result.account?.id) throw new Error(t('connect.unconfirmed')); if (mounted) connected(); }
		catch (error) {
			if (mounted) failure = (error as Error).message;
			try {
				const result = await management.oauth(attempt.id);
				if (mounted && session?.id === attempt.id) {
					terminal = ['error','expired'].includes(result.status);
					if (terminal) callback = '';
					requiredAction(attempt, result);
				}
			} catch { /* Keep the original connection error if status is unavailable. */ }
		}
		finally { pending = false; }
	}
	async function saveCredential(event: SubmitEvent) {
		event.preventDefault(); pending = true; failure = '';
		try { const result = await management.importCredential({ type:manual,name:accountName,access_token:credential,base_url:endpoint,...(custom ? { route:route.trim().toLowerCase(),...(dialect ? { dialect } : {}) } : {}) }); if (result.errors?.length || !result.imported) throw new Error(result.errors?.length ? importErrors(result.errors) : t('connect.notAdded')); if (mounted) connected(); }
		catch (error) { if (mounted) failure = (error as Error).message; }
		finally { pending = false; }
	}
	function choose(provider: Provider) {
		const mode = connectMode(provider);
		if (mode === 'oauth') { void start(provider.id); return; }
		failure = '';
		if (mode === 'google-setup') { setup = 'antigravity'; void loadSetupClient(); return; }
		manual = provider.id; endpoint = provider.base_url ?? endpoints[provider.id] ?? ''; accountName = mode === 'endpoint' ? '' : providerName(provider.id, provider.name);
	}
	const optionKeys = { oauth: 'connect.oauthOption', 'google-setup': 'connect.googleSetupOption', endpoint: 'connect.apiKeyOption', token: 'connect.tokenOption' } as const;
	function reset() { session = null; setup = ''; manual = ''; credential = ''; callback = ''; failure = ''; status = ''; terminal = false; route = ''; dialect = 'chat'; detecting = false; cmdCopied = false; setupClient = null; confirmDetect = false; }
	async function loadSetupClient() {
		// Without it the detect button stays, and the server reports what goes wrong.
		try { const { clients } = await management.oauthClients(); if (mounted && setup) setupClient = clients.find(client => client.provider === 'antigravity') ?? null; }
		catch { /* Keep the guide as it is. */ }
	}
	async function detectClient(force = false) {
		if (detecting) return;
		failure = '';
		if (!force && detectNeedsConfirm(setupClient)) { confirmDetect = true; return; }
		detecting = true; confirmDetect = false;
		try { await management.detectAntigravityClient({ force }); if (mounted) { setup = ''; await start('antigravity'); } }
		catch (error) { if (mounted) failure = (error as Error).message; }
		finally { if (mounted) detecting = false; }
	}
	async function copyCommand() { try { await copyText(detectCommand); cmdCopied = true; if (failure === t('connect.copyRefused')) failure = ''; } catch { cmdCopied = false; failure = t('connect.copyRefused'); } }
	async function copyCode() { if (!session?.user_code) return; try { await copyText(session.user_code); codeCopied = true; if (failure === t('connect.copyRefused')) failure = ''; } catch { codeCopied = false; failure = t('connect.copyRefused'); } }
</script>

<dialog bind:this={dialog} oncancel={close} class="connect-dialog">
	<div class="dialog-heading"><div><h2>{t('common.connectAccount')}</h2><p>{t('connect.subtitle')}</p></div><button class="icon-button" onclick={close} aria-label={t('common.close')}><X size={18}/></button></div>
	{#if success}
		<div class="connection-success"><span><Check size={24}/></span><h3>{t('connect.success')}</h3><p>{t('connect.successText')}</p><button class="button primary" onclick={close}>{t('connect.viewAccounts')}</button></div>
	{:else if session}
		{#if session.verification_required}
			<div class="oauth-flow">
				<p>{t('connect.googleVerify')}</p>
				{#if session.verification_available}<a class="button primary" href={`/admin/api/oauth/${encodeURIComponent(session.id)}/verification`} target="_blank" rel="noopener noreferrer" data-sveltekit-reload>{t('common.verifyGoogle')}<ExternalLink size={16}/></a>{:else}<p>{t('common.openAntigravity')}</p>{/if}
				<p>{t('connect.afterVerify')}</p>
				<div class="flow-controls"><button class="button" disabled={pending} onclick={() => session && start(session.provider)}>{t('connect.restart')}</button><button class="button subtle" disabled={pending} onclick={reset}>{t('common.chooseOtherProvider')}</button></div>
			</div>
		{:else if session.provider === 'antigravity' && session.error_code === 'google_project_unavailable'}
			<div class="oauth-flow" role="status">
				<h3>{t('connect.antigravityTitle')}</h3>
				<p>{t('connect.antigravityProject')}</p>
				<p>{t('connect.antigravityFirstUse')}</p>
				<a class="button" href="https://antigravity.google/docs/getting-started" target="_blank" rel="noopener noreferrer external">{t('connect.antigravityGuide')}<ExternalLink size={16}/></a>
				<div class="flow-controls"><button class="button primary" disabled={pending} onclick={() => session && start(session.provider)}>{t('connect.restart')}</button><button class="button subtle" disabled={pending} onclick={reset}>{t('common.chooseOtherProvider')}</button></div>
			</div>
		{:else}
		<div class="oauth-flow">
			{#if session.user_code}<p>{t('connect.enterCode', { provider: providerName(session.provider) })}</p><div class="device-code"><span>{session.user_code}</span><button class="icon-button" onclick={copyCode} aria-label={t('connect.copyCode')}>{#if codeCopied}<Check size={16}/>{:else}<Copy size={16}/>{/if}</button></div>{:else}<p>{t('connect.authorize')}</p>{/if}
			<a class="button primary" href={session.url} target="_blank" rel="noopener noreferrer external" data-sveltekit-reload>{t('connect.open')}<ExternalLink size={14}/></a>
			{#if !session.user_code}<p class="flow-hint">{t(callbackHintKey(session.provider))}</p><form onsubmit={finish}><label for="callback">{t('connect.callbackLabel')}</label><input id="callback" type="text" placeholder={t('connect.callbackPlaceholder')} bind:value={callback} required maxlength="8192" autocomplete="off" spellcheck="false"/><button class="button primary" disabled={pending || terminal || !callback.trim()}>{t('connect.finish')}</button></form>{/if}
			<div class="flow-controls"><button class="button" disabled={pending || terminal} onclick={() => check()}>{t('connect.check')}</button><button class="button subtle" disabled={pending} onclick={reset}>{t('common.chooseOtherProvider')}</button></div>{#if status}<p class="muted small" role="status">{status}</p>{/if}
		</div>
		{/if}
	{:else if setup}
		<div class="oauth-flow google-setup">
			<h3>{t('connect.googleSetupTitle', { provider: providerName(setup) })}</h3>
			<p>{t('connect.googleSetupIntro', { provider: providerName(setup) })}</p>
			<ol class="setup-steps">
				<li>
					<h4>{t('connect.googleSetupDetectTitle')}</h4>
					{#if setupClient && !setupClient.detectable}
						<p>{t('connect.googleSetupDetectUnavailable')}</p>
					{:else if confirmDetect}
						<p>{t('connect.googleSetupDetectText')}</p>
						<p class="replace-note" role="note">{t('googleClients.replaceWarning')}</p>
						<div class="setup-actions"><button class="button danger" disabled={detecting} onclick={() => detectClient(true)}><Radar size={15}/>{detecting ? t('connect.googleSetupDetecting') : t('connect.googleSetupReplace')}</button><button class="button" disabled={detecting} onclick={() => confirmDetect = false}>{t('common.cancel')}</button></div>
					{:else}
						<p>{t('connect.googleSetupDetectText')}</p>
						<button class="button primary" disabled={detecting} onclick={() => detectClient()}><Radar size={15}/>{detecting ? t('connect.googleSetupDetecting') : t('connect.googleSetupDetect')}</button>
					{/if}
				</li>
				<li>
					<h4>{t('connect.googleSetupRemoteTitle')}</h4>
					<p>{t('connect.googleSetupRemoteText')}</p>
					<div class="setup-command"><code>{detectCommand}</code><button class="icon-button" onclick={copyCommand} aria-label={t('connect.googleSetupCopy')}>{#if cmdCopied}<Check size={16}/>{:else}<Copy size={16}/>{/if}</button></div>
					<p class="setup-note">{t('connect.googleSetupRemoteToken')}</p>
				</li>
				<li>
					<h4>{t('connect.googleSetupPasteTitle')}</h4>
					<p>{t('connect.googleSetupPasteText')}</p>
					<button class="button" disabled={detecting} onclick={openGoogleClients}><Settings2 size={15}/>{t('connect.googleSetupSettings')}</button>
				</li>
				<li>
					<h4>{t('connect.googleSetupImportTitle')}</h4>
					<p>{t('connect.googleSetupImportText')}</p>
					<button class="button" disabled={detecting} onclick={() => fileInput?.click()}><Upload size={14}/>{t('connect.googleSetupImport')}</button>
				</li>
			</ol>
			<div class="flow-controls"><button class="button subtle" disabled={detecting} onclick={reset}>{t('common.chooseOtherProvider')}</button></div>
		</div>
	{:else if manual && custom}
		<div class="oauth-flow"><p>{t('connect.customIntro1')}<code>deepseek</code>{t('connect.customIntro2')}<code>{t('connect.customModel')}</code>{t('connect.customIntro3')}</p><form onsubmit={saveCredential}><label for="account-name">{t('connect.name')}</label><input id="account-name" bind:value={accountName} placeholder="DeepSeek" required/><label for="route">{t('endpoint.route')}</label><input id="route" bind:value={route} required pattern="[a-z0-9][a-z0-9_\-]{'{0,31}'}" placeholder="deepseek" autocomplete="off" spellcheck="false"/><label for="endpoint">{t('endpoint.url')}</label><input id="endpoint" type="url" bind:value={endpoint} required placeholder="https://api.deepseek.com/v1"/><label for="dialect">{t('endpoint.format')}</label><select id="dialect" bind:value={dialect}>{#each dialects as option (option)}<option value={option}>{dialectLabel(option)}</option>{/each}</select><label for="credential">{t('endpoint.key')}</label><input id="credential" type="password" bind:value={credential} required autocomplete="off" spellcheck="false"/><button class="button primary" disabled={pending || !credential.trim() || !route.trim()}>{t('connect.addEndpoint')}</button></form><button class="button subtle" style="margin-top:15px" disabled={pending} onclick={reset}>{t('common.chooseOtherProvider')}</button></div>
	{:else if manual}
		<div class="oauth-flow"><p>{t('connect.tokenIntro')}</p><form onsubmit={saveCredential}><label for="account-name">{t('connect.accountName')}</label><input id="account-name" bind:value={accountName} placeholder={t('connect.accountNamePlaceholder')} required/><label for="credential">{t('connect.token')}</label><input id="credential" type="password" bind:value={credential} required autocomplete="off" spellcheck="false"/><label for="endpoint">{t('connect.providerUrl')}</label><input id="endpoint" type="url" bind:value={endpoint} required placeholder={t('connect.providerUrlPlaceholder')}/><button class="button primary" disabled={pending || !credential.trim()}>{t('connect.addAccount')}</button></form><button class="button subtle" style="margin-top:15px" disabled={pending} onclick={reset}>{t('common.chooseOtherProvider')}</button></div>
	{:else}
		<div class="provider-options">{#each providers as provider (provider.id)}<button class="provider-option" disabled={pending} onclick={() => choose(provider)}><span class="provider-logo"><ProviderMark provider={provider.id} size={28}/></span><span>{providerName(provider.id, provider.name)}<small>{t(optionKeys[connectMode(provider)])}</small></span><ChevronRight size={15}/></button>{/each}{#if !providers.length}<p class="muted small">{t('connect.noProvider')}</p>{/if}</div>
		<div class="dialog-import"><div><h3>{t('connect.haveFile')}</h3><p>{t('connect.importFiles')}</p></div><button class="button" disabled={pending} onclick={() => fileInput?.click()}><Upload size={14}/>{t('common.import')}</button></div>
	{/if}
	<input hidden type="file" accept=".json,application/json" multiple bind:this={fileInput} onchange={(event) => { if (event.currentTarget.files?.length) { importFiles(event.currentTarget.files); close(); } }}/>
	{#if pending}<p class="muted small" role="status">{t('connect.pending')}</p>{/if}
	{#if failure}<div class="notice error" role="alert">{failure}</div>{/if}
</dialog>

<style>
	.connect-dialog { width: min(640px,calc(100vw - 32px)); max-height: calc(100dvh - 32px); overflow-y: auto; padding: 28px; color: var(--text); background: var(--surface); border: 1px solid #35496b; border-radius: 18px; box-shadow: 0 24px 80px #030711b0; }.connect-dialog::backdrop { background: #030711b5; backdrop-filter: blur(6px); }
	.dialog-heading { position: sticky; top: 0; z-index: 2; display: flex; gap: 20px; align-items: flex-start; justify-content: space-between; padding-bottom: 18px; margin-bottom: 8px; background: var(--surface); }.dialog-heading h2 { font-family: var(--font-display); font-size: 26px; font-weight: 650; letter-spacing: -.035em; margin: 0 0 8px; }.dialog-heading p,.dialog-import p { color: var(--muted); font-size: 15px; line-height: 1.5; margin: 0; }
	.provider-options { display: grid; grid-template-columns: repeat(2,minmax(0,1fr)); gap: 11px; }.provider-option { display: flex; gap: 12px; align-items: center; min-height: 86px; padding: 15px 12px; border: 1px solid var(--border); border-radius: 12px; background: var(--background); text-align: left; }.provider-option:hover { background: var(--surface-raised); border-color: #5576ad; }.provider-option>span:not(.provider-logo) { flex: 1; font-size: 15px; font-weight: 550; }.provider-logo { display: flex; align-items: center; justify-content: center; width: 36px; flex-shrink: 0; }.provider-option small { display: block; color: var(--muted); font-size: 15px; font-weight: 400; line-height: 1.5; margin-top: 5px; }.provider-option :global(svg) { color: var(--faint); flex-shrink: 0; }
	.dialog-import { margin-top: 24px; padding-top: 22px; border-top: 1px solid var(--border); display: flex; gap: 18px; align-items: center; }.dialog-import h3 { font-size: 15px; margin: 0 0 6px; }
	.oauth-flow>p { font-size: 15px; color: var(--muted); line-height: 1.6; }.oauth-flow .flow-hint { margin-top: 24px; font-size: 15px; }.oauth-flow form { display: grid; gap: 10px; }.oauth-flow label { font-size: 15px; }.oauth-flow input,.oauth-flow select { width: 100%; }.oauth-flow code { font-size: 14px; }.flow-controls { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 18px; }
	.google-setup .setup-steps { display: grid; grid-template-columns: minmax(0,1fr); gap: 14px; margin: 18px 0 0; padding: 0; list-style: none; counter-reset: step; }
	.google-setup .setup-steps>li { position: relative; padding: 15px 16px 15px 48px; border: 1px solid var(--border); border-radius: 12px; background: var(--background); counter-increment: step; }
	.google-setup .setup-steps>li::before { content: counter(step); position: absolute; left: 14px; top: 15px; display: grid; place-items: center; width: 22px; height: 22px; font-size: 13px; font-weight: 600; color: var(--muted); border: 1px solid var(--border); border-radius: 50%; }
	.google-setup .setup-steps h4 { margin: 0 0 4px; font-size: 15px; font-weight: 600; }
	.google-setup .setup-steps p { margin: 0 0 11px; font-size: 14px; color: var(--muted); line-height: 1.55; }
	.google-setup .setup-steps :global(.button) { width: 100%; justify-content: center; }
	.setup-command { display: flex; align-items: center; gap: 8px; padding: 9px 11px; border: 1px solid var(--border); border-radius: 9px; background: var(--surface); }
	.setup-command code { flex: 1; min-width: 0; overflow-x: auto; white-space: nowrap; font-size: 13px; }
	.setup-command .icon-button { flex-shrink: 0; }
	.google-setup .setup-steps .setup-note { margin: 11px 0 0; }
	.google-setup .setup-steps .replace-note { color: var(--text); }
	.setup-actions { display: flex; gap: 8px; }
	.setup-actions :global(.button) { flex: 1; }
	.device-code { display: flex; align-items: center; justify-content: space-between; border: 1px solid var(--border); background: var(--background); border-radius: 10px; padding: 14px 17px; margin: 20px 0; }.device-code>span { font-family: var(--font-display); font-size: 28px; letter-spacing: .08em; }
	.connection-success { padding: 15px 0; text-align: center; }.connection-success>span { display: inline-grid; place-items: center; width: 50px; height: 50px; background: #10b98115; color: var(--green); border-radius: 50%; }.connection-success h3 { font-size: 18px; }.connection-success p { color: var(--muted); font-size: 15px; margin-bottom: 24px; }
	@media(max-width:600px) { .connect-dialog { padding: 22px 18px; border-radius: 15px; }.dialog-heading h2 { font-size: 24px; }.provider-options { grid-template-columns: 1fr; gap: 9px; }.provider-option { min-height: 75px; padding: 13px 15px; gap: 15px; }.provider-option>span:not(.provider-logo) { font-size: 15px; }.provider-option small { font-size: 15px; }.dialog-import { align-items: flex-start; flex-direction: column; } }
</style>
