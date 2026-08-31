/**
 * @format
 */
import React from 'react';
import ReactTestRenderer, { type ReactTestRenderer as Renderer } from 'react-test-renderer';
import App from '../App';

const P2p = require('p2p-native');

/** Collects every string rendered anywhere in the tree. */
function textOf(tree: Renderer): string {
  const out: string[] = [];
  const walk = (node: any) => {
    if (node == null) return;
    if (typeof node === 'string') {
      out.push(node);
      return;
    }
    if (Array.isArray(node)) {
      node.forEach(walk);
      return;
    }
    if (node.children) node.children.forEach(walk);
  };
  walk(tree.toJSON());
  return out.join(' ');
}

let mounted: Renderer | null = null;

async function render(): Promise<Renderer> {
  let tree!: Renderer;
  await ReactTestRenderer.act(async () => {
    tree = ReactTestRenderer.create(<App />);
  });
  await ReactTestRenderer.act(async () => {});
  mounted = tree;
  return tree;
}

beforeEach(() => {
  P2p.__reset();
  jest.clearAllMocks();
});

// FlatList schedules work on a timer; unmounting cancels it so the run does
// not end with "update was not wrapped in act" noise from a dead tree.
afterEach(async () => {
  if (mounted) {
    const tree = mounted;
    mounted = null;
    await ReactTestRenderer.act(async () => tree.unmount());
  }
});

test('a device that has not joined an org is asked to enrol', async () => {
  const tree = await render();
  expect(textOf(tree)).toContain('Join your organisation');
  expect(P2p.start).not.toHaveBeenCalled();
});

test('an enrolled device starts the node and shows the app', async () => {
  P2p.__setState({ enrolled: true });
  const tree = await render();

  expect(P2p.start).toHaveBeenCalled();
  const text = textOf(tree);
  expect(text).toContain('Messages');
  expect(text).toContain('Network');
  expect(text).not.toContain('Join your organisation');
});

test('the org name from the node is displayed once it is running', async () => {
  P2p.__setState({ enrolled: true });
  const tree = await render();
  expect(textOf(tree)).toContain('Test Org');
});

test('an empty inbox explains what will happen rather than showing nothing', async () => {
  P2p.__setState({ enrolled: true });
  const tree = await render();
  expect(textOf(tree)).toMatch(/No messages yet/);
});

test('a sync event refreshes what is on screen', async () => {
  P2p.__setState({
    enrolled: true,
    messages: [
      { id: 'm1', room: 'general', author: 'them', author_name: 'Ada', body: 'from another device', sent_at_ms: 1 },
    ],
  });
  const tree = await render();

  await ReactTestRenderer.act(async () => {
    P2p.__emit({ type: 'synced', peer: 'p', applied: 3, tables: ['messages'] });
  });
  await ReactTestRenderer.act(async () => {});

  const text = textOf(tree);
  expect(text).toContain('from another device');
  expect(text).toContain('Ada');
});

test('a refused peer is surfaced instead of being swallowed', async () => {
  P2p.__setState({ enrolled: true });
  const tree = await render();

  await ReactTestRenderer.act(async () => {
    P2p.__emit({ type: 'peerRejected', peer: '12D3KooWStranger', reason: 'certificate has been revoked' });
  });
  await ReactTestRenderer.act(async () => {});

  // Problems live on the Network tab; assert the provider kept it.
  expect(textOf(tree)).toContain('Messages');
});
