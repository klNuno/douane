// The ban-risk warning shows once per browser, until accepted.
const storageKey = 'douane-risk-accepted';

export function riskAccepted(storage: Pick<Storage, 'getItem'> | undefined = globalThis.localStorage): boolean {
	try { return storage?.getItem(storageKey) === '1'; }
	catch { return false; }
}

export function acceptRisk(storage: Pick<Storage, 'setItem'> | undefined = globalThis.localStorage): void {
	try { storage?.setItem(storageKey, '1'); }
	catch { /* A browser without storage shows the warning again on the next visit. */ }
}
