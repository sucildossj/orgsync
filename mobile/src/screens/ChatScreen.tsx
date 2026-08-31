import React, { useCallback, useEffect, useRef, useState } from 'react';
import {
  FlatList,
  KeyboardAvoidingView,
  Platform,
  Pressable,
  Text,
  View,
} from 'react-native';
import * as P2p from 'p2p-native';
import type { Message } from 'p2p-native';
import { useP2p } from '../P2pProvider';
import { useTheme } from '../theme';
import { Empty, Field } from '../ui';

const ROOM = 'general';

export function ChatScreen() {
  const t = useTheme();
  const { revision, peerId, status } = useP2p();
  const [items, setItems] = useState<Message[]>([]);
  const [draft, setDraft] = useState('');
  const [sending, setSending] = useState(false);

  const listRef = useRef<FlatList<Message>>(null);

  const load = useCallback(async () => {
    try {
      // `messages()` returns newest first; the list renders oldest to newest.
      setItems((await P2p.messages(ROOM)).slice().reverse());
    } catch {
      // The node may not be up yet; the next revision will retry.
    }
  }, []);

  // `revision` ticks on every local write and every batch merged from a peer.
  useEffect(() => {
    void load();
  }, [load, revision]);

  async function send() {
    const body = draft.trim();
    if (!body || sending) return;
    setSending(true);
    setDraft('');
    try {
      await P2p.sendMessage(ROOM, body);
      await load();
    } catch {
      setDraft(body); // hand it back rather than losing what they typed
    } finally {
      setSending(false);
    }
  }

  return (
    <KeyboardAvoidingView
      style={{ flex: 1 }}
      behavior={Platform.OS === 'ios' ? 'padding' : undefined}
      keyboardVerticalOffset={Platform.OS === 'ios' ? 88 : 0}>
      <FlatList
        ref={listRef}
        data={items}
        keyExtractor={m => m.id}
        contentContainerStyle={{ padding: 16, gap: 10, flexGrow: 1 }}
        onContentSizeChange={() => listRef.current?.scrollToEnd({ animated: false })}
        onLayout={() => listRef.current?.scrollToEnd({ animated: false })}
        ListEmptyComponent={
          <View style={{ flex: 1, justifyContent: 'center' }}>
            <Empty>
              {status?.peers.length
                ? 'No messages yet. Say something — it will reach every device in the org.'
                : 'No messages yet. Messages are stored locally and sync as soon as another device is reachable.'}
            </Empty>
          </View>
        }
        renderItem={({ item }) => <Bubble message={item} mine={item.author === peerId} />}
      />

      <View
        style={{
          flexDirection: 'row',
          gap: 10,
          padding: 12,
          borderTopWidth: 1,
          borderTopColor: t.border,
          backgroundColor: t.card,
        }}>
        <Field
          value={draft}
          onChangeText={setDraft}
          placeholder={`Message #${ROOM}`}
          style={{ flex: 1 }}
          multiline
          onSubmitEditing={send}
        />
        <Pressable
          onPress={send}
          disabled={!draft.trim() || sending}
          style={({ pressed }) => ({
            backgroundColor: t.accent,
            opacity: !draft.trim() || sending ? 0.4 : pressed ? 0.85 : 1,
            borderRadius: 10,
            paddingHorizontal: 18,
            justifyContent: 'center',
          })}>
          <Text style={{ color: t.accentText, fontWeight: '600' }}>Send</Text>
        </Pressable>
      </View>
    </KeyboardAvoidingView>
  );
}

function Bubble({ message, mine }: { message: Message; mine: boolean }) {
  const t = useTheme();
  return (
    <View style={{ alignItems: mine ? 'flex-end' : 'flex-start' }}>
      {!mine ? (
        <Text style={{ color: t.faint, fontSize: 11, marginBottom: 3, marginLeft: 6 }}>
          {message.author_name || 'unknown device'}
        </Text>
      ) : null}
      <View
        style={{
          backgroundColor: mine ? t.bubbleMine : t.bubbleTheirs,
          borderRadius: 16,
          paddingHorizontal: 14,
          paddingVertical: 10,
          maxWidth: '82%',
        }}>
        <Text style={{ color: mine ? '#fff' : t.text, fontSize: 16, lineHeight: 21 }}>{message.body}</Text>
      </View>
      <Text style={{ color: t.faint, fontSize: 10, marginTop: 3, marginHorizontal: 6 }}>
        {formatTime(message.sent_at_ms)}
      </Text>
    </View>
  );
}

function formatTime(ms: number): string {
  if (!ms) return '';
  return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}
