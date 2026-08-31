import React, { useState } from 'react';
import { Platform, RefreshControl, ScrollView, Text, View } from 'react-native';
import { useP2p } from '../P2pProvider';
import { useTheme } from '../theme';
import { Button, Card, Dot, Empty, Label } from '../ui';

const mono = Platform.OS === 'ios' ? 'Menlo' : 'monospace';

export function StatusScreen() {
  const t = useTheme();
  const { status, running, peerId, problems, syncNow, refreshStatus } = useP2p();
  const [refreshing, setRefreshing] = useState(false);

  const reachable = (status?.externalAddrs.length ?? 0) > 0;

  return (
    <ScrollView
      contentContainerStyle={{ padding: 16, gap: 14 }}
      refreshControl={
        <RefreshControl
          refreshing={refreshing}
          tintColor={t.dim}
          onRefresh={async () => {
            setRefreshing(true);
            await refreshStatus();
            setRefreshing(false);
          }}
        />
      }>
      <Card style={{ gap: 12 }}>
        <View style={{ flexDirection: 'row', alignItems: 'center', gap: 8 }}>
          <Dot on={running} />
          <Text style={{ color: t.text, fontSize: 18, fontWeight: '700' }}>
            {status?.orgName || (running ? 'Running' : 'Starting…')}
          </Text>
        </View>
        <Row label="You" value={status?.displayName || '—'} />
        <Row label="Device" value={peerId ? `${peerId.slice(0, 10)}…${peerId.slice(-6)}` : '—'} mono />
        <Row label="Connected" value={`${status?.peers.length ?? 0} of ${status?.connections ?? 0}`} />
        <Row label="Replicated changes" value={String(status?.changes ?? 0)} />
        <Row label="Devices known" value={String(status?.knownDevices ?? 0)} />
        {status?.certExpiresAtMs ? (
          <Row label="Access expires" value={new Date(status.certExpiresAtMs).toLocaleDateString()} />
        ) : null}
      </Card>

      <Card style={{ gap: 10 }}>
        <Label>Peers</Label>
        {status?.peers.length ? (
          status.peers.map(p => (
            <View key={p.peerId} style={{ flexDirection: 'row', alignItems: 'center', gap: 10 }}>
              <Dot on />
              <View style={{ flex: 1 }}>
                <Text style={{ color: t.text, fontSize: 15 }}>{p.displayName}</Text>
                <Text style={{ color: t.faint, fontSize: 11 }}>
                  {p.userId} · {p.role}
                </Text>
              </View>
            </View>
          ))
        ) : (
          <Empty>
            No other devices right now. On a shared network they are found automatically; otherwise
            they connect through the seed server.
          </Empty>
        )}
      </Card>

      <Card style={{ gap: 8 }}>
        <Label>Reachability</Label>
        <Text style={{ color: t.dim, fontSize: 13, lineHeight: 19 }}>
          {reachable
            ? 'This device has an address other devices can reach from outside this network.'
            : 'No external address yet. Devices on this network can still reach you directly.'}
        </Text>
        {status?.listenAddrs.slice(0, 4).map(a => (
          <Text key={a} style={{ color: t.faint, fontSize: 10, fontFamily: mono }} numberOfLines={1}>
            {a}
          </Text>
        ))}
      </Card>

      {problems.length ? (
        <Card style={{ gap: 6 }}>
          <Label>Recent problems</Label>
          {problems.slice(0, 6).map((p, i) => (
            <Text key={`${i}-${p}`} style={{ color: t.warn, fontSize: 12, lineHeight: 18 }}>
              {p}
            </Text>
          ))}
        </Card>
      ) : null}

      <Button title="Sync now" tone="plain" onPress={() => void syncNow().catch(() => {})} />
    </ScrollView>
  );
}

function Row({ label, value, mono: isMono }: { label: string; value: string; mono?: boolean }) {
  const t = useTheme();
  return (
    <View style={{ flexDirection: 'row', justifyContent: 'space-between', gap: 12 }}>
      <Text style={{ color: t.dim, fontSize: 14 }}>{label}</Text>
      <Text
        style={{ color: t.text, fontSize: 14, fontFamily: isMono ? mono : undefined, flexShrink: 1 }}
        numberOfLines={1}>
        {value}
      </Text>
    </View>
  );
}
