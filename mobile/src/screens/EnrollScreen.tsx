import React, { useState } from 'react';
import { KeyboardAvoidingView, Platform, ScrollView, Text, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { useP2p } from '../P2pProvider';
import { useTheme } from '../theme';
import { Button, Card, Field, Label, Notice } from '../ui';

export function EnrollScreen() {
  const t = useTheme();
  const insets = useSafeAreaInsets();
  const { enroll, peerId } = useP2p();
  const [seedUrl, setSeedUrl] = useState('');
  const [inviteCode, setInviteCode] = useState('');
  const [deviceName, setDeviceName] = useState(Platform.OS === 'ios' ? 'My iPhone' : 'My Android');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canSubmit = seedUrl.trim().length > 0 && inviteCode.trim().length > 0 && !busy;

  async function submit() {
    setBusy(true);
    setError(null);
    try {
      await enroll({ seedUrl, inviteCode, deviceName: deviceName.trim() || 'device' });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <KeyboardAvoidingView
      style={{ flex: 1 }}
      behavior={Platform.OS === 'ios' ? 'padding' : undefined}>
      <ScrollView
        contentContainerStyle={{ padding: 20, paddingTop: insets.top + 20, gap: 16 }}
        keyboardShouldPersistTaps="handled">
        <View style={{ gap: 6, marginTop: 12 }}>
          <Text style={{ color: t.text, fontSize: 28, fontWeight: '700' }}>Join your organisation</Text>
          <Text style={{ color: t.dim, fontSize: 15, lineHeight: 22 }}>
            An admin gives you a one-time code. This device makes its own key and never sends it
            anywhere — the server only signs the public half.
          </Text>
        </View>

        <Card style={{ gap: 16 }}>
          <View>
            <Label>Server address</Label>
            <Field
              value={seedUrl}
              onChangeText={setSeedUrl}
              placeholder="https://your-org.example.com"
              autoCapitalize="none"
              autoCorrect={false}
              keyboardType="url"
              inputMode="url"
            />
          </View>

          <View>
            <Label>Invite code</Label>
            <Field
              value={inviteCode}
              onChangeText={setInviteCode}
              placeholder="4KP7M-9XQ2T-…"
              autoCapitalize="characters"
              autoCorrect={false}
            />
          </View>

          <View>
            <Label>Device name</Label>
            <Field value={deviceName} onChangeText={setDeviceName} placeholder="My phone" />
          </View>

          {error ? <Notice kind="error">{error}</Notice> : null}

          <Button title="Join" onPress={submit} busy={busy} disabled={!canSubmit} />
        </Card>

        {peerId ? (
          <View style={{ paddingHorizontal: 4 }}>
            <Label>This device</Label>
            <Text style={{ color: t.faint, fontSize: 12, fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace' }}>
              {peerId}
            </Text>
          </View>
        ) : null}
      </ScrollView>
    </KeyboardAvoidingView>
  );
}
