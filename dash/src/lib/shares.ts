/** Reads the share fields of the client dialog: a percentage per pool or account, null when one is invalid.
 * 0 or an empty field removes the share. */
export function parseShares(draft: Record<string, string>): Record<string, number> | null {
	const next: Record<string, number> = {};
	for (const [id, input] of Object.entries(draft)) {
		const text = (input ?? '').trim().replace(',', '.');
		if (!text) continue;
		const value = Number(text);
		if (!/^\d+(\.\d+)?$/.test(text) || value > 100) return null;
		if (value > 0) next[id] = value;
	}
	return next;
}
