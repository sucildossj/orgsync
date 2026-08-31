/**
 * The app-facing API.
 *
 * Everything is a thin promise wrapper over the native module, except
 * `enroll`, which also makes the HTTP call. That split is deliberate: React
 * Native's `fetch` does the networking, and Rust does the cryptography, so no
 * TLS stack has to be compiled into the mobile binary.
 */

import { NativeEventEmitter, NativeModules, Platform } from 'react-native';
import type {
  Enrollment,
  Message,
  NodeStatus,
  OrgRecord,
  P2pEvent,
  P2pInit,
  SqlParam,
} from './types';

export * from './types';

const LINKING_ERROR =
  `The p2p-native module is not linked.\n` +
  (Platform.OS === 'ios'
    ? `Run scripts/build-ios.sh, then 'cd mobile/ios && pod install'.`
    : `Run scripts/build-android.sh, then rebuild the app.`);

const Native = NativeModules.P2pNative
  ? NativeModules.P2pNative
  : new Proxy(
      {},
      {
        get() {
          throw new Error(LINKING_ERROR);
        },
      },
    );

const emitter = new NativeEventEmitter(NativeModules.P2pNative);

/** An error raised by the Rust core. `message` is safe to show to a person. */
export class P2pError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'P2pError';
  }
}

function rethrow(e: unknown): never {
  const message = e instanceof Error ? e.message : String(e);
  throw new P2pError(message);
}

async function call<T>(fn: () => Promise<T>): Promise<T> {
  try {
    return await fn();
  } catch (e) {
    return rethrow(e);
  }
}

export interface InitOptions {
  /** Defaults to a private directory inside the app sandbox. */
  dataDir?: string;
  /** Find peers on the local network. Leave on. */
  enableMdns?: boolean;
  /** Use a seed server as a relay while behind NAT. */
  enableRelay?: boolean;
}

/** Opens the local replica, creating this device's key on first run. */
export function initialize(options: InitOptions = {}): Promise<P2pInit> {
  return call(() => Native.initialize(options));
}

/** Subscribes to node events. Returns an unsubscribe function. */
export function addListener(handler: (event: P2pEvent) => void): () => void {
  const sub = emitter.addListener('p2p', (raw: string) => {
    try {
      handler(JSON.parse(raw) as P2pEvent);
    } catch {
      // A malformed event must never take the app down.
    }
  });
  return () => sub.remove();
}

export interface EnrollOptions {
  /** Base URL of the seed server, e.g. https://xyz.trycloudflare.com */
  seedUrl: string;
  /** The code from an admin. Dashes and case are ignored. */
  inviteCode: string;
  /** How this device will appear to the rest of the org. */
  deviceName: string;
}

/**
 * Joins an organisation.
 *
 * The request body is built and signed in Rust (it proves this device holds
 * the key it wants certified), posted from JS, and the reply is verified in
 * Rust against the org key before anything is stored.
 */
export async function enroll(options: EnrollOptions): Promise<Enrollment> {
  const base = options.seedUrl.trim().replace(/\/+$/, '');
  const body: string = await call(() =>
    Native.beginEnrollment(options.inviteCode, options.deviceName),
  );

  let response: Response;
  try {
    response = await fetch(`${base}/v1/enroll`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body,
    });
  } catch {
    throw new P2pError(`Could not reach ${base}. Check the address and your connection.`);
  }

  const text = await response.text();
  if (!response.ok) {
    // The server explains refusals in plain language; prefer that message.
    let reason = `The server refused the invite (HTTP ${response.status}).`;
    try {
      const parsed = JSON.parse(text);
      if (parsed?.error) reason = parsed.error;
    } catch {
      /* keep the generic message */
    }
    throw new P2pError(reason);
  }

  const enrollment: string = await call(() => Native.completeEnrollment(text));
  return JSON.parse(enrollment) as Enrollment;
}

/** Starts the node. Requires a completed enrolment. */
export function start(): Promise<boolean> {
  return call(() => Native.start());
}

export function stop(): Promise<boolean> {
  return call(() => Native.stop());
}

export async function status(): Promise<NodeStatus> {
  const raw: string = await call(() => Native.status());
  return JSON.parse(raw) as NodeStatus;
}

/** Runs anti-entropy with every connected peer immediately. */
export function syncNow(): Promise<boolean> {
  return call(() => Native.syncNow());
}

/** Connects to a specific peer or seed server by multiaddr. */
export function dial(multiaddr: string): Promise<boolean> {
  return call(() => Native.dial(multiaddr));
}

/** Runs a read query. Returns rows as objects keyed by column name. */
export async function query<T = Record<string, unknown>>(
  sql: string,
  params: SqlParam[] = [],
): Promise<T[]> {
  const raw: string = await call(() => Native.query(sql, JSON.stringify(params)));
  return JSON.parse(raw) as T[];
}

/**
 * Runs a write. The change is captured, stamped and pushed to connected peers
 * before this resolves, so edits show up on other devices right away.
 */
export function execute(sql: string, params: SqlParam[] = []): Promise<number> {
  return call(() => Native.execute(sql, JSON.stringify(params)));
}

/** Posts a message. It is an ordinary replicated row. Returns its id. */
export function sendMessage(room: string, body: string): Promise<string> {
  return call(() => Native.sendMessage(room, body));
}

/**
 * Brings one of your own tables into replication. It must already exist and
 * have a single-column primary key — use an opaque id, because the primary key
 * is what identifies a row across devices.
 */
export function registerTable(table: string, pkColumn = 'id'): Promise<boolean> {
  return call(() => Native.registerTable(table, pkColumn));
}

/** Convenience reader for the built-in chat table. */
export function messages(room = 'general', limit = 200): Promise<Message[]> {
  return query<Message>(
    `SELECT id, room, author, author_name, body, sent_at_ms
     FROM messages WHERE room = ?1 ORDER BY sent_at_ms DESC, id DESC LIMIT ?2`,
    [room, limit],
  );
}

/** Convenience reader for the built-in records table. */
export function records(collection = 'default'): Promise<OrgRecord[]> {
  return query<OrgRecord>(
    `SELECT id, collection, title, body, status, updated_by, updated_at_ms
     FROM records WHERE collection = ?1 ORDER BY updated_at_ms DESC`,
    [collection],
  );
}
