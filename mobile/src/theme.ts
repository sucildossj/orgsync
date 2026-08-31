import { useColorScheme } from 'react-native';

const light = {
  bg: '#f6f7f9',
  card: '#ffffff',
  border: '#e2e5ea',
  text: '#14181f',
  dim: '#6b7280',
  faint: '#9ca3af',
  accent: '#2f6fed',
  accentText: '#ffffff',
  good: '#0f8a54',
  warn: '#b45309',
  bad: '#c02626',
  bubbleMine: '#2f6fed',
  bubbleTheirs: '#eceef2',
};

const dark: typeof light = {
  bg: '#0d1117',
  card: '#161b23',
  border: '#262d38',
  text: '#e8eaed',
  dim: '#9aa4b2',
  faint: '#6b7482',
  accent: '#5b8dff',
  accentText: '#0d1117',
  good: '#3fb984',
  warn: '#e0a355',
  bad: '#f26d6d',
  bubbleMine: '#2f5fbd',
  bubbleTheirs: '#232a35',
};

export type Theme = typeof light;

export function useTheme(): Theme {
  return useColorScheme() === 'dark' ? dark : light;
}
