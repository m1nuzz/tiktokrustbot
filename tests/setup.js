// Global test setup for Telegram Mini App tests
// Mock Telegram WebApp API

import { vi } from 'vitest';

// Mock Telegram WebApp global object
const mockTelegramWebApp = {
  openLink: vi.fn(),
  close: vi.fn(),
  ready: vi.fn(),
  expand: vi.fn(),
  MainButton: {
    show: vi.fn(),
    hide: vi.fn(),
    showProgress: vi.fn(),
    hideProgress: vi.fn(),
    setText: vi.fn(),
    onClick: vi.fn(),
    offClick: vi.fn()
  },
  HapticFeedback: {
    notificationOccurred: vi.fn()
  },
  themeParams: {
    button_color: '#38bdf8'
  },
  initDataUnsafe: {
    user: {
      id: 12345
    }
  }
};

// Mock global fetch
global.fetch = vi.fn();

// Mock Telegram global
global.Telegram = {
  WebApp: mockTelegramWebApp
};

// Reset all mocks before each test
beforeEach(() => {
  vi.clearAllMocks();
  global.fetch.mockReset();
});

// Export for use in tests
export { mockTelegramWebApp };
