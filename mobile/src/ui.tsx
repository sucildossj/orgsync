import React from 'react';
import {
  ActivityIndicator,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
  type TextInputProps,
  type ViewProps,
} from 'react-native';
import { useTheme } from './theme';

export function Card({ style, ...rest }: ViewProps) {
  const t = useTheme();
  return (
    <View
      {...rest}
      style={[
        { backgroundColor: t.card, borderColor: t.border, borderWidth: StyleSheet.hairlineWidth, borderRadius: 14, padding: 16 },
        style,
      ]}
    />
  );
}

export function Label({ children }: { children: React.ReactNode }) {
  const t = useTheme();
  return (
    <Text style={{ color: t.dim, fontSize: 12, fontWeight: '600', letterSpacing: 0.6, textTransform: 'uppercase', marginBottom: 6 }}>
      {children}
    </Text>
  );
}

export function Field(props: TextInputProps) {
  const t = useTheme();
  return (
    <TextInput
      placeholderTextColor={t.faint}
      {...props}
      style={[
        {
          backgroundColor: t.bg,
          borderColor: t.border,
          borderWidth: StyleSheet.hairlineWidth,
          borderRadius: 10,
          paddingHorizontal: 12,
          paddingVertical: 12,
          color: t.text,
          fontSize: 16,
        },
        props.style,
      ]}
    />
  );
}

export function Button({
  title,
  onPress,
  busy,
  disabled,
  tone = 'accent',
}: {
  title: string;
  onPress: () => void;
  busy?: boolean;
  disabled?: boolean;
  tone?: 'accent' | 'plain';
}) {
  const t = useTheme();
  const off = disabled || busy;
  const bg = tone === 'accent' ? t.accent : t.card;
  return (
    <Pressable
      onPress={onPress}
      disabled={off}
      style={({ pressed }) => ({
        backgroundColor: bg,
        opacity: off ? 0.5 : pressed ? 0.85 : 1,
        borderRadius: 10,
        paddingVertical: 14,
        alignItems: 'center',
        borderWidth: tone === 'plain' ? StyleSheet.hairlineWidth : 0,
        borderColor: t.border,
      })}>
      {busy ? (
        <ActivityIndicator color={tone === 'accent' ? t.accentText : t.text} />
      ) : (
        <Text style={{ color: tone === 'accent' ? t.accentText : t.text, fontWeight: '600', fontSize: 16 }}>
          {title}
        </Text>
      )}
    </Pressable>
  );
}

export function Notice({ kind, children }: { kind: 'error' | 'info'; children: React.ReactNode }) {
  const t = useTheme();
  const color = kind === 'error' ? t.bad : t.dim;
  return (
    <View style={{ borderLeftWidth: 3, borderLeftColor: color, paddingLeft: 10, paddingVertical: 4 }}>
      <Text style={{ color, fontSize: 14, lineHeight: 20 }}>{children}</Text>
    </View>
  );
}

export function Empty({ children }: { children: React.ReactNode }) {
  const t = useTheme();
  return (
    <View style={{ padding: 32, alignItems: 'center' }}>
      <Text style={{ color: t.faint, textAlign: 'center', lineHeight: 22 }}>{children}</Text>
    </View>
  );
}

export function Dot({ on }: { on: boolean }) {
  const t = useTheme();
  return <View style={{ width: 8, height: 8, borderRadius: 4, backgroundColor: on ? t.good : t.faint }} />;
}
