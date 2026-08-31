// react-native-safe-area-context measures the window before it renders its
// children, which never happens in a test renderer — without this the whole
// tree comes out empty. The library ships a mock with fixed insets for exactly
// this case.
jest.mock('react-native-safe-area-context', () =>
  require('react-native-safe-area-context/jest/mock').default,
);
