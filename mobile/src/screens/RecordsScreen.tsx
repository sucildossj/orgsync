import React, { useCallback, useEffect, useState } from 'react';
import { FlatList, Pressable, RefreshControl, Text, View } from 'react-native';
import * as P2p from 'p2p-native';
import type { OrgRecord } from 'p2p-native';
import { useP2p } from '../P2pProvider';
import { useTheme } from '../theme';
import { Button, Card, Empty, Field } from '../ui';

/**
 * Demonstrates the part that is not chat: an ordinary shared table.
 *
 * Nothing here knows about the network. Rows are written with plain SQL and
 * the replica takes care of getting them everywhere — including the case where
 * two people edit different fields of the same row while apart, which merges
 * instead of one overwriting the other.
 */
export function RecordsScreen() {
  const t = useTheme();
  const { revision, syncNow } = useP2p();
  const [items, setItems] = useState<OrgRecord[]>([]);
  const [title, setTitle] = useState('');
  const [refreshing, setRefreshing] = useState(false);

  const load = useCallback(async () => {
    try {
      setItems(await P2p.records());
    } catch {
      /* node not up yet */
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load, revision]);

  async function add() {
    const text = title.trim();
    if (!text) return;
    setTitle('');
    await P2p.execute(
      `INSERT INTO records (id, collection, title, status, updated_at_ms)
       VALUES (?1, 'default', ?2, 'open', ?3)`,
      [newId(), text, Date.now()],
    );
    await load();
  }

  async function toggle(item: OrgRecord) {
    await P2p.execute(`UPDATE records SET status = ?2, updated_at_ms = ?3 WHERE id = ?1`, [
      item.id,
      item.status === 'done' ? 'open' : 'done',
      Date.now(),
    ]);
    await load();
  }

  async function remove(item: OrgRecord) {
    await P2p.execute(`DELETE FROM records WHERE id = ?1`, [item.id]);
    await load();
  }

  return (
    <FlatList
      data={items}
      keyExtractor={r => r.id}
      contentContainerStyle={{ padding: 16, gap: 10 }}
      refreshControl={
        <RefreshControl
          refreshing={refreshing}
          tintColor={t.dim}
          onRefresh={async () => {
            setRefreshing(true);
            await syncNow().catch(() => {});
            await load();
            setRefreshing(false);
          }}
        />
      }
      ListHeaderComponent={
        <Card style={{ gap: 12, marginBottom: 6 }}>
          <Field value={title} onChangeText={setTitle} placeholder="Add a shared item…" onSubmitEditing={add} />
          <Button title="Add" onPress={add} disabled={!title.trim()} />
        </Card>
      }
      ListEmptyComponent={
        <Empty>Nothing here yet. Anything you add is shared with every device in the org.</Empty>
      }
      renderItem={({ item }) => (
        <Pressable onLongPress={() => remove(item)} onPress={() => toggle(item)}>
          <Card style={{ flexDirection: 'row', alignItems: 'center', gap: 12 }}>
            <View
              style={{
                width: 22,
                height: 22,
                borderRadius: 11,
                borderWidth: 2,
                borderColor: item.status === 'done' ? t.good : t.border,
                backgroundColor: item.status === 'done' ? t.good : 'transparent',
                alignItems: 'center',
                justifyContent: 'center',
              }}>
              {item.status === 'done' ? (
                <Text style={{ color: '#fff', fontSize: 13, fontWeight: '700' }}>✓</Text>
              ) : null}
            </View>
            <View style={{ flex: 1 }}>
              <Text
                style={{
                  color: t.text,
                  fontSize: 16,
                  textDecorationLine: item.status === 'done' ? 'line-through' : 'none',
                  opacity: item.status === 'done' ? 0.55 : 1,
                }}>
                {item.title}
              </Text>
              <Text style={{ color: t.faint, fontSize: 11, marginTop: 2 }}>
                {item.updated_at_ms ? new Date(item.updated_at_ms).toLocaleString() : ''}
              </Text>
            </View>
          </Card>
        </Pressable>
      )}
    />
  );
}

function newId(): string {
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
