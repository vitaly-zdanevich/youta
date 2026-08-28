import assert from 'node:assert/strict';
import test from 'node:test';

import { subscriptionPageRows } from './subscriptionPageRows.ts';

test('subscription page capacity counts only complete virtual rows', () => {
	assert.equal(subscriptionPageRows(480), 10);
	assert.equal(subscriptionPageRows(479), 9);
	assert.equal(subscriptionPageRows(48), 1);
	assert.equal(subscriptionPageRows(47), 1);
	assert.equal(subscriptionPageRows(0), null);
});
