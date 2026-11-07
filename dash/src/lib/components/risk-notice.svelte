<script lang="ts">
	import Modal from './modal.svelte';
	import { t } from '$lib/i18n.svelte';
	let { accept }: { accept: () => void } = $props();
</script>

<Modal title={t('risk.title')} close={accept} dismissible={false}>
	<p class="dialog-text">{t('risk.body')}</p>
	<p class="dialog-text">{t('risk.warranty')}</p>
	<div class="dialog-actions"><button class="button primary" onclick={accept}>{t('risk.accept')}</button></div>
</Modal>
