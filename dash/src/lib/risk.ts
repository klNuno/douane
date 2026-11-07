// The ban-risk warning shows once per browser, until accepted.
const storageKey = 'douane-risk-accepted';

// Reading `localStorage` itself throws when the browser blocks storage, so it stays inside the try.
export function riskAccepted(storage?: Pick<Storage, 'getItem'>): boolean {
	try { return (storage ?? globalThis.localStorage)?.getItem(storageKey) === '1'; }
	catch { return false; }
}

export function acceptRisk(storage?: Pick<Storage, 'setItem'>): void {
	try { (storage ?? globalThis.localStorage)?.setItem(storageKey, '1'); }
	catch { /* A browser without storage shows the warning again on the next visit. */ }
}
