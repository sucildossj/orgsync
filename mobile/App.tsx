/**
 * OrgSync — a peer-to-peer, offline-first client for an organisation's shared
 * SQLite database.
 *
 * The app talks only to the local replica. Getting rows to and from other
 * devices is the Rust core's job, and it happens whether or not any server is
 * reachable.
 */
import React, { useEffect, useState } from 'react';
import {
  ActivityIndicator,
  Keyboard,
  Platform,
  Pressable,
  StatusBar,
  StyleSheet,
  Text,
  View,
  useColorScheme,
} from 'react-native';
import { SafeAreaProvider, useSafeAreaInsets } from 'react-native-safe-area-context';
import { P2pProvider, useP2p } from './src/P2pProvider';
import { ChatScreen } from './src/screens/ChatScreen';
import { EnrollScreen } from './src/screens/EnrollScreen';
import { RecordsScreen } from './src/screens/RecordsScreen';
import { StatusScreen } from './src/screens/StatusScreen';
import { useTheme } from './src/theme';
import { Dot, Notice } from './src/ui';

type Tab = 'chat' | 'records' | 'status';
const TABS: { key: Tab; label: string }[] = [
  { key: 'chat', label: 'Messages' },
  { key: 'records', label: 'Shared' },
  { key: 'status', label: 'Network' },
];

/**
 * How tall the on-screen keyboard is, or 0 when it is closed.
 *
 * Android 15 runs apps edge to edge, so the window is no longer resized for the
 * keyboard and `adjustResize` / `KeyboardAvoidingView` leave content underneath
 * it. Measuring the keyboard directly works on every version. iOS keeps using
 * `KeyboardAvoidingView`, so this reports 0 there.
 *
 * The reported frame stops at the navigation bar, so callers add the bottom
 * safe-area inset to reach the true top of the keyboard.
 */
function useKeyboardInset(): number {
  const [height, setHeight] = useState(0);
  useEffect(() => {
    if (Platform.OS !== 'android') return;
    const show = Keyboard.addListener('keyboardDidShow', e =>
      setHeight(e.endCoordinates.height),
    );
    const hide = Keyboard.addListener('keyboardDidHide', () => setHeight(0));
    return () => {
      show.remove();
      hide.remove();
    };
  }, []);
  return height;
}

function Shell() {
  const t = useTheme();
  const insets = useSafeAreaInsets();
  const { ready, enrolled, running, status, error } = useP2p();
  const [tab, setTab] = useState<Tab>('chat');
  const keyboard = useKeyboardInset();

  if (!ready) {
    return (
      <View style={[styles.center, { backgroundColor: t.bg }]}>
        <ActivityIndicator color={t.dim} />
      </View>
    );
  }

  // Before enrolling there is nothing else to show, so the error takes the
  // whole screen. Once enrolled the app is still usable offline, so it goes
  // above the tabs instead — but it must never be hidden, or a node that
  // failed to start just reads as a permanent "Starting…".
  if (error && !enrolled) {
    return (
      <View style={[styles.center, { backgroundColor: t.bg, padding: 24 }]}>
        <Notice kind="error">{error}</Notice>
      </View>
    );
  }

  if (!enrolled) return <EnrollScreen />;

  return (
    <View style={{ flex: 1, paddingBottom: keyboard > 0 ? keyboard + insets.bottom : 0 }}>
      <View
        style={{
          flexDirection: 'row',
          alignItems: 'center',
          gap: 8,
          paddingHorizontal: 16,
          paddingTop: insets.top + 6,
          paddingBottom: 10,
          borderBottomWidth: StyleSheet.hairlineWidth,
          borderBottomColor: t.border,
        }}>
        <Dot on={running && (status?.peers.length ?? 0) > 0} />
        <Text style={{ color: t.text, fontSize: 17, fontWeight: '700', flex: 1 }} numberOfLines={1}>
          {status?.orgName || 'OrgSync'}
        </Text>
        <Text style={{ color: t.faint, fontSize: 12 }}>
          {status?.peers.length ? `${status.peers.length} online` : running ? 'alone' : 'offline'}
        </Text>
      </View>

      {error ? (
        <View style={{ paddingHorizontal: 16, paddingTop: 10 }}>
          <Notice kind="error">{error}</Notice>
        </View>
      ) : null}

      <View style={{ flex: 1 }}>
        {tab === 'chat' ? <ChatScreen /> : tab === 'records' ? <RecordsScreen /> : <StatusScreen />}
      </View>

      <View
        style={{
          flexDirection: 'row',
          borderTopWidth: StyleSheet.hairlineWidth,
          borderTopColor: t.border,
          backgroundColor: t.card,
          paddingBottom: insets.bottom,
          // While the keyboard is up the composer needs the room more.
          display: keyboard > 0 ? 'none' : 'flex',
        }}>
        {TABS.map(item => (
          <Pressable
            key={item.key}
            onPress={() => setTab(item.key)}
            style={({ pressed }) => ({
              flex: 1,
              paddingVertical: 14,
              alignItems: 'center',
              opacity: pressed ? 0.6 : 1,
            })}>
            <Text
              style={{
                color: tab === item.key ? t.accent : t.dim,
                fontWeight: tab === item.key ? '700' : '500',
                fontSize: 13,
              }}>
              {item.label}
            </Text>
          </Pressable>
        ))}
      </View>
    </View>
  );
}

export default function App() {
  const isDark = useColorScheme() === 'dark';
  const t = useTheme();
  return (
    <SafeAreaProvider>
      <View style={{ flex: 1, backgroundColor: t.bg }}>
        <StatusBar
          barStyle={isDark ? 'light-content' : 'dark-content'}
          backgroundColor={Platform.OS === 'android' ? t.bg : undefined}
        />
        <P2pProvider>
          <Shell />
        </P2pProvider>
      </View>
    </SafeAreaProvider>
  );
}

const styles = StyleSheet.create({
  center: { flex: 1, alignItems: 'center', justifyContent: 'center' },
});
