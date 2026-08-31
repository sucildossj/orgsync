/** Types mirroring what the Rust core emits. Kept in one place so a change
 *  to the protocol shows up here as a type error rather than at runtime. */

export type Role = 'admin' | 'member' | 'readonly';

/** What `initialize()` reports back about this device. */
export interface P2pInit {
  peerId: string;
  enrolled: boolean;
  running: boolean;
  /** The SQLite file. Safe to open read-only with another driver. */
  dbPath: string;
}

/** Events pushed from the node. Discriminated on `type`. */
export type P2pEvent =
  | { type: 'started'; peerId: string; orgId: string }
  | { type: 'listening'; addr: string }
  /** We have a relayed address, so peers outside this network can reach us. */
  | { type: 'relayReserved'; addr: string }
  | { type: 'peerConnected'; peer: string; userId: string; displayName: string; role: Role }
  | { type: 'peerDisconnected'; peer: string }
  /** A device tried to connect and was refused. `reason` is safe to display. */
  | { type: 'peerRejected'; peer: string; reason: string }
  /** Remote changes landed; the local tables have already been updated. */
  | { type: 'synced'; peer: string; applied: number; tables: string[] }
  | { type: 'localChanges'; count: number }
  | { type: 'stopped' }
  | { type: 'error'; message: string };

export interface PeerSummary {
  peerId: string;
  userId: string;
  displayName: string;
  role: Role;
  sinceMs: number;
}

export interface NodeStatus {
  peerId: string;
  orgId: string;
  orgName: string;
  displayName: string;
  listenAddrs: string[];
  externalAddrs: string[];
  connections: number;
  peers: PeerSummary[];
  /** Total replicated changes held locally. */
  changes: number;
  /** How many devices this replica has ever heard from. */
  knownDevices: number;
  certExpiresAtMs: number;
}

export interface CertClaims {
  org_id: string;
  device_pub: string;
  user_id: string;
  display_name: string;
  role: Role;
  issued_at_ms: number;
  expires_at_ms: number;
  serial: number;
}

export interface Enrollment {
  org_id: string;
  org_name: string;
  cert: { claims: CertClaims; org_pub: string; signature: string };
  bootstrap: string[];
}

/** A row of the built-in `messages` table. Keys are the SQL column names. */
export interface Message {
  id: string;
  room: string;
  /** PeerId of the device that wrote it. */
  author: string;
  author_name: string;
  body: string;
  sent_at_ms: number;
}

/** A row of the built-in `records` table. */
export interface OrgRecord {
  id: string;
  collection: string;
  title: string;
  body: string;
  status: string;
  updated_by: string;
  updated_at_ms: number;
}

/** A SQL parameter. Binary is passed as `{ $blob: '<hex>' }`. */
export type SqlParam = string | number | boolean | null | { $blob: string };
