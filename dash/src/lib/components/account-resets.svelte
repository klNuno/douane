<script lang="ts">
	import { untrack } from 'svelte';
	import RefreshCw from '@lucide/svelte/icons/refresh-cw';
	import X from '@lucide/svelte/icons/x';
	import { management, privateLabel, formatDate, newResetRequestId, type Account, type ResetAvailability, type ResetKind, type ResetScope } from '$lib/management';
	import { t, type StringKey } from '$lib/i18n.svelte';
	let { account, busy, updated }: { account: Account; busy: string; updated: () => Promise<void> } = $props();
	let availability = $state.raw<ResetAvailability | null>(null);
	let checking = $state(false);
	let saving = $state(false);
	let failure = $state('');
	let result = $state('');
	let pending = $state(false);
	let confirmation = $state('');
	let requestId = '';
	let kinds = $derived(availability?.kinds ?? []);
	// Resets that clear different things are never picked for the user.
	let scope = $state<ResetScope | null>(null);
	let chosen = $derived(kinds.find(kind => kind.scope === scope) ?? null);
	let ready = $derived(confirmation === 'reset' && (pending || !kinds.length || !!chosen));
	/** A button's name: the week, or the model family the provider's label names. */
	function kindName(kind: ResetKind) {
		if (kind.scope === 'account') return account.provider === 'claude' ? t('resets.weekly') : t('resets.all');
		return ['Fable', 'Opus', 'Sonnet', 'Haiku'].find(family => kind.label?.includes(family)) ?? t('resets.model');
	}
	let dialog = $state<HTMLDialogElement>();
	let supported = $derived(account.enabled && ['claude', 'codex'].includes(account.provider));
	let storageKey = $derived(`douane-reset-${account.id}`);
	let canReset = $derived(availability?.status === 'ready' && (availability.available_count ?? 0) > 0);
	async function check() {
		if (!supported || checking) return;
		checking = true; failure = '';
		try { availability = await management.accountResets(account.id); if (availability.pending_request_id) { sessionStorage.setItem(storageKey, availability.pending_request_id); pending = true; } }
		catch (error) { failure = privateLabel((error as Error).message); }
		finally { checking = false; }
	}
	$effect(() => { if (supported) untrack(() => { pending = !!sessionStorage.getItem(storageKey); void check(); }); else availability = null; });
	function open(kind: ResetScope | null = null) {
		requestId = availability?.pending_request_id ?? sessionStorage.getItem(storageKey) ?? newResetRequestId();
		failure = ''; result = ''; confirmation = ''; scope = kind; dialog?.showModal();
	}
	async function redeem() {
		if (saving || busy || !ready || !dialog?.open) return;
		saving = true; failure = '';
		try {
			// A retry settles the reset already picked; only a new attempt chooses.
			const wanted = pending ? undefined : chosen?.scope;
			sessionStorage.setItem(storageKey, requestId); pending = true;
			const response = await management.redeemReset(account.id, requestId, wanted);
			const messages: Record<string, StringKey> = { reset: 'resets.reset', already_redeemed: 'resets.alreadyRedeemed', nothing_to_reset: 'resets.nothingToReset', no_credit: 'resets.noCredit' };
			const message = messages[response.outcome];
			if (!message) throw new Error(t('resets.unconfirmed'));
			result = t(message);
			sessionStorage.removeItem(storageKey); pending = false;
			await check(); await updated();
		} catch (error) { failure = privateLabel((error as Error).message); }
		finally { saving = false; }
	}
</script>

{#if supported}
	<div class="reset-control">
		{#if pending}<button class="button reset-button" disabled={!!busy || checking} onclick={() => open()}><RefreshCw size={17}/>{t('resets.resume')}</button>
		{:else if canReset && kinds.length}{#each kinds as kind (kind.scope)}<button class="button reset-button" disabled={!!busy || checking} title={kind.label ?? t(`resets.scope.${kind.scope}Info`)} aria-label={t('resets.useNamed', { count: kind.count, name: kindName(kind) })} onclick={() => open(kind.scope)}><RefreshCw size={17}/><strong class="tabular">{kind.count}</strong>{kindName(kind)}</button>{/each}
		{:else if canReset}<button class="button reset-button" disabled={!!busy || checking} onclick={() => open()}><RefreshCw size={17}/>{t('resets.button', { count: availability?.available_count ?? 0 })}</button>
		{:else if checking}<span class="reset-status">{t('resets.checking')}</span>
		{:else if availability?.status === 'ready'}<span class="reset-status">{t('resets.none')}</span>
		{:else if availability?.status !== 'unsupported'}<button class="reset-retry" disabled={!!busy} onclick={check}>{t('resets.check')}</button>{/if}
		{#if failure && !dialog?.open}<p class="reset-error" role="alert">{failure}</p>{/if}
		{#if availability?.error && !failure && availability.status !== 'unsupported'}<p class="reset-error">{privateLabel(availability.error)}</p>{/if}
	</div>
	<dialog bind:this={dialog} oncancel={(event) => { if (saving) event.preventDefault(); }}>
		<div class="reset-dialog">
			<header><h2>{t('resets.title')}</h2><button class="icon-button" aria-label={t('resets.closeConfirmation')} disabled={saving} onclick={() => dialog?.close()}><X size={22}/></button></header>
			<p class="reset-account">{privateLabel(account.name)}</p>
			{#if result}<p class="reset-result" role="status">{result}</p><div class="dialog-actions"><button class="button primary" onclick={() => dialog?.close()}>{t('common.close')}</button></div>
			{:else}
				<p>{t('resets.warning')}</p>
				{#if !pending && chosen}
					<div class="reset-kind"><strong>{kindName(chosen)}</strong><small>{t(`resets.scope.${chosen.scope}Info`)}</small>{#if chosen.label}<small class="provider-label">{chosen.label}</small>{/if}<small>{t('resets.count', { count: chosen.count })}{#if chosen.next_expires_at}{` · ${t('resets.expiresFirst', { date: formatDate(chosen.next_expires_at) })}`}{/if}</small></div>
				{:else if availability?.next_expires_at}<p>{t('resets.expiresFirst', { date: formatDate(availability.next_expires_at) })}</p>{/if}
				{#if pending}<p>{t('resets.pending')}</p>{/if}
				{#if failure}<p class="reset-error" role="alert">{failure}</p>{/if}
				<form onsubmit={(event) => { event.preventDefault(); void redeem(); }}>
					<label class="reset-confirmation">{t('resets.typeConfirmation')}<input bind:value={confirmation} required autocomplete="off" autocapitalize="none" spellcheck={false} disabled={saving}/></label>
					<div class="dialog-actions"><button type="button" class="button" disabled={saving} onclick={() => dialog?.close()}>{t('common.cancel')}</button><button type="submit" class="button primary" disabled={saving || !!busy || !ready}>{saving ? t('resets.running') : pending ? t('resets.retrySame') : t('resets.use')}</button></div>
				</form>
			{/if}
		</div>
	</dialog>
{/if}

<style>
	.reset-confirmation { display: grid; gap: 10px; font-size: 15px; margin-top: 20px; }.reset-confirmation input { width: 100%; }
	.reset-kind { margin-top: 16px; padding: 14px; border: 1px solid var(--border); border-radius: 12px; background: var(--background); }.reset-kind strong { display: block; color: var(--text); font-size: 17px; font-weight: 600; }.reset-kind small { display: block; margin-top: 5px; color: var(--muted); font-size: 14px; line-height: 1.5; }.reset-kind .provider-label { color: var(--faint); font-style: italic; }.reset-button strong { font-weight: 700; }
	.reset-control { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-top: 15px; }
	.reset-button { font-size: 15px; padding: 8px 11px; min-height: 42px; }
	.reset-status { font-size: 15px; color: var(--muted); }
	.reset-retry { padding: 4px 0; border: 0; background: transparent; color: var(--blue); font-size: 15px; }
	.reset-error { flex-basis: 100%; font-size: 15px; color: var(--red); line-height: 1.5; margin: 6px 0; overflow-wrap: anywhere; }
	dialog { max-width: min(520px,calc(100vw - 32px)); width: 100%; padding: 0; border: 1px solid var(--border); border-radius: 16px; background: var(--surface); color: var(--text); box-shadow: 0 24px 100px #0008; }
	dialog::backdrop { background: #030711b5; backdrop-filter: blur(5px); }
	.reset-dialog { padding: 25px; }
	.reset-dialog header { display: flex; align-items: center; justify-content: space-between; gap: 15px; }
	.reset-dialog h2 { font-family: var(--font-display); font-size: 24px; }
	.reset-dialog p { font-size: 16px; color: var(--muted); line-height: 1.6; }
	.reset-dialog .reset-account { color: var(--text); font-weight: 600; }
	.reset-dialog .reset-error { color: var(--red); }
	.reset-dialog .reset-result { color: var(--green); }
	.dialog-actions { display: flex; flex-wrap: wrap; justify-content: flex-end; gap: 10px; margin-top: 24px; }
	@media(max-width:650px) { .reset-dialog { padding: 20px; }.dialog-actions .button { flex: 1; }.reset-dialog h2 { font-size: 22px; } }
</style>
