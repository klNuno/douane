import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseShares } from '../src/lib/shares.ts';

test('a share set to 0 or left empty is removed, not refused', () => {
	assert.deepEqual(parseShares({ lzt: '0', main: '92', empty: ' ', comma: '2,5', zero: '0.0' }), { main: 92, comma: 2.5 });
});

test('a share outside 0 to 100 or not a number is refused', () => {
	for (const input of ['101', '-1', 'abc', '1e2', 'NaN']) assert.equal(parseShares({ a: input }), null, input);
	assert.deepEqual(parseShares({ a: '100' }), { a: 100 });
});
