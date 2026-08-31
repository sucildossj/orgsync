/**
 * A fake node for the app's tests.
 *
 * Jest picks this up automatically for the `p2p-native` package. It keeps a
 * little in-memory state so the tests exercise the real provider logic —
 * enrolling, starting, receiving events — without any native code.
 */
let state = {
  enrolled: false,
  running: false,
  messages: [],
  records: [],
};
const listeners = new Set();

function emit(event) {
  for (const fn of listeners) fn(event);
}

const initialize = jest.fn(async () => ({
  peerId: '12D3KooWTestPeerIdForUnitTests',
  enrolled: state.enrolled,
  running: state.running,
  dbPath: '/tmp/test/replica.db',
}));

const addListener = jest.fn(handler => {
  listeners.add(handler);
  return () => listeners.delete(handler);
});

const enroll = jest.fn(async () => {
  state.enrolled = true;
  return { org_id: 'testorg', org_name: 'Test Org', bootstrap: [] };
});

const start = jest.fn(async () => {
  state.running = true;
  emit({ type: 'started', peerId: 'p', orgId: 'testorg' });
  return true;
});

const stop = jest.fn(async () => {
  state.running = false;
  return true;
});

const status = jest.fn(async () => ({
  peerId: '12D3KooWTestPeerIdForUnitTests',
  orgId: 'testorg',
  orgName: 'Test Org',
  displayName: 'Test Device',
  listenAddrs: [],
  externalAddrs: [],
  connections: 0,
  peers: [],
  changes: 0,
  knownDevices: 1,
  certExpiresAtMs: Date.now() + 86400000,
}));

const messages = jest.fn(async () => state.messages);
const records = jest.fn(async () => state.records);

const sendMessage = jest.fn(async (room, body) => {
  const id = `m${state.messages.length + 1}`;
  state.messages = [
    { id, room, author: 'me', author_name: 'Me', body, sent_at_ms: Date.now() },
    ...state.messages,
  ];
  return id;
});

module.exports = {
  __setState: next => {
    state = { ...state, ...next };
  },
  __emit: emit,
  __reset: () => {
    state = { enrolled: false, running: false, messages: [], records: [] };
    listeners.clear();
  },
  initialize,
  addListener,
  enroll,
  start,
  stop,
  status,
  syncNow: jest.fn(async () => true),
  dial: jest.fn(async () => true),
  query: jest.fn(async () => []),
  execute: jest.fn(async () => 1),
  registerTable: jest.fn(async () => true),
  messages,
  records,
  sendMessage,
  P2pError: class P2pError extends Error {},
};
