/**
 * One place that owns the node's lifecycle and its event stream.
 *
 * Screens read state from here rather than talking to the native module
 * directly, so there is exactly one `initialize()` and one event subscription
 * for the whole app.
 */
import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import { AppState } from 'react-native';
import * as P2p from 'p2p-native';
import type { NodeStatus, P2pEvent } from 'p2p-native';

interface P2pState {
  ready: boolean;
  enrolled: boolean;
  running: boolean;
  peerId: string;
  status: NodeStatus | null;
  /** Rejected peers and node errors, newest first. Useful and reassuring. */
  problems: string[];
  /** Bumped whenever replicated data changes, so lists can refetch. */
  revision: number;
  error: string | null;
  enroll: (opts: P2p.EnrollOptions) => Promise<void>;
  refreshStatus: () => Promise<void>;
  syncNow: () => Promise<void>;
}

const Ctx = createContext<P2pState | null>(null);

export function useP2p(): P2pState {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error('useP2p must be used inside <P2pProvider>');
  return ctx;
}

export function P2pProvider({ children }: { children: React.ReactNode }) {
  const [ready, setReady] = useState(false);
  const [enrolled, setEnrolled] = useState(false);
  const [running, setRunning] = useState(false);
  const [peerId, setPeerId] = useState('');
  const [status, setStatus] = useState<NodeStatus | null>(null);
  const [problems, setProblems] = useState<string[]>([]);
  const [revision, setRevision] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const started = useRef(false);

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await P2p.status());
    } catch {
      // Not running yet; the next event or poll will pick it up.
    }
  }, []);

  const startNode = useCallback(async () => {
    if (started.current) return;
    started.current = true;
    try {
      await P2p.start();
      setRunning(true);
      await refreshStatus();
    } catch (e) {
      started.current = false;
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [refreshStatus]);

  // Boot once.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const init = await P2p.initialize({});
        if (cancelled) return;
        setPeerId(init.peerId);
        setEnrolled(init.enrolled);
        setReady(true);
        if (init.enrolled) await startNode();
      } catch (e) {
        if (!cancelled) {
          setError(e instanceof Error ? e.message : String(e));
          setReady(true);
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [startNode]);

  // The single event subscription for the app.
  useEffect(() => {
    return P2p.addListener((event: P2pEvent) => {
      switch (event.type) {
        case 'started':
          setRunning(true);
          void refreshStatus();
          break;
        case 'stopped':
          setRunning(false);
          break;
        case 'synced':
          // Rows have already been merged into the local tables.
          setRevision(r => r + 1);
          void refreshStatus();
          break;
        case 'localChanges':
          setRevision(r => r + 1);
          break;
        case 'peerConnected':
        case 'peerDisconnected':
        case 'listening':
        case 'relayReserved':
          void refreshStatus();
          break;
        case 'peerRejected':
          setProblems(p => [`Refused ${event.peer.slice(0, 12)}…: ${event.reason}`, ...p].slice(0, 20));
          break;
        case 'error':
          setProblems(p => [event.message, ...p].slice(0, 20));
          break;
      }
    });
  }, [refreshStatus]);

  // Coming back from the background is the moment a phone is most likely to
  // be behind on other devices' changes.
  useEffect(() => {
    const sub = AppState.addEventListener('change', state => {
      if (state === 'active' && started.current) {
        void P2p.syncNow().catch(() => {});
        void refreshStatus();
      }
    });
    return () => sub.remove();
  }, [refreshStatus]);

  const enroll = useCallback(
    async (opts: P2p.EnrollOptions) => {
      await P2p.enroll(opts);
      setEnrolled(true);
      setError(null);
      await startNode();
    },
    [startNode],
  );

  const syncNow = useCallback(async () => {
    await P2p.syncNow();
    await refreshStatus();
    setRevision(r => r + 1);
  }, [refreshStatus]);

  const value = useMemo(
    () => ({ ready, enrolled, running, peerId, status, problems, revision, error, enroll, refreshStatus, syncNow }),
    [ready, enrolled, running, peerId, status, problems, revision, error, enroll, refreshStatus, syncNow],
  );

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
