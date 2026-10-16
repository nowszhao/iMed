import { test } from 'node:test';
import assert from 'node:assert/strict';
import { bindSearchInput } from './search-input.js';

function setup(t) {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const input = new EventTarget();
  input.value = '';
  const queries = [];
  let invalidations = 0;
  const cancel = bindSearchInput(input, query => queries.push(query), () => invalidations++);
  const emit = (type, value = input.value, composing = false) => {
    input.value = value;
    const event = new Event(type);
    event.isComposing = composing;
    input.dispatchEvent(event);
  };
  return { input, queries, emit, cancel, invalidations: () => invalidations };
}

test('Chinese composition searches only the committed word 头发', t => {
  const { queries, emit } = setup(t);
  emit('compositionstart');
  emit('input', 'tou', true);
  t.mock.timers.tick(500);
  assert.deepEqual(queries, []);
  emit('input', '头fa', true);
  t.mock.timers.tick(500);
  assert.deepEqual(queries, []);
  emit('compositionend', '头发');
  emit('input', '头发');
  t.mock.timers.tick(450);
  assert.deepEqual(queries, ['头发']);
});

test('editing invalidates an in-flight search before the debounce expires', t => {
  const { emit, invalidations } = setup(t);
  emit('input', '头');
  t.mock.timers.tick(450);
  const before = invalidations();
  emit('input', '头发');
  assert.ok(invalidations() > before);
});

test('starting composition cancels a pending query', t => {
  const { emit, queries } = setup(t);
  emit('input', '头');
  emit('compositionstart');
  t.mock.timers.tick(500);
  assert.deepEqual(queries, []);
});

test('paste is trimmed and rapid edits produce only the latest query', t => {
  const { emit, queries } = setup(t);
  emit('input', '头');
  t.mock.timers.tick(200);
  emit('input', '  头发  ');
  t.mock.timers.tick(450);
  assert.deepEqual(queries, ['头发']);
});

test('navigation cancels the pending search', t => {
  const { emit, queries, cancel } = setup(t);
  emit('input', '头发');
  cancel();
  t.mock.timers.tick(500);
  assert.deepEqual(queries, []);
});
