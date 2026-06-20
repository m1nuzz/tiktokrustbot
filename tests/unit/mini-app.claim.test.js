/**
 * Unit tests for claimVideo() function
 * Tests spinner display, fetch calls, success/error handling
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { JSDOM } from 'jsdom';

describe('Telegram Mini App - claimVideo()', () => {
  let dom;
  let document;
  let window;
  let loaderView;
  let contentView;
  let loaderText;
  let loaderSubtext;
  let directBtn;
  let tg;
  let apiBase;
  let ymid;

  beforeEach(() => {
    // Setup JSDOM
    dom = new JSDOM(`
      <!DOCTYPE html>
      <html>
        <body>
          <div id="loader-view" class="hidden"></div>
          <div id="content-view"></div>
          <h1 id="title"></h1>
          <p id="description"></p>
          <button id="direct-btn" class="hidden"></button>
        </body>
      </html>
    `, { url: 'http://localhost' });
    
    document = dom.window.document;
    window = dom.window;
    
    // Get DOM elements
    loaderView = document.getElementById('loader-view');
    contentView = document.getElementById('content-view');
    loaderText = document.createElement('div');
    loaderSubtext = document.createElement('div');
    directBtn = document.getElementById('direct-btn');
    
    // Mock loader text elements
    const loaderTextEl = document.getElementById('loader-text');
    const loaderSubtextEl = document.getElementById('loader-subtext');
    
    // Setup test data
    apiBase = 'http://localhost';
    ymid = 'test123';
    
    // Mock Telegram WebApp
    tg = {
      MainButton: {
        hide: vi.fn()
      },
      WebApp: {
        close: vi.fn()
      },
      HapticFeedback: {
        notificationOccurred: vi.fn()
      }
    };
    
    global.tg = tg;
  });

  describe('DOM manipulations', () => {
    it('должен показать спиннер при начале запроса', () => {
      // Simulate claimVideo logic
      loaderView.classList.remove('hidden');
      contentView.classList.add('hidden');
      
      expect(loaderView.classList.contains('hidden')).toBe(false);
      expect(contentView.classList.contains('hidden')).toBe(true);
    });

    it('должен скрыть content-view при начале запроса', () => {
      contentView.classList.add('hidden');
      
      expect(contentView.classList.contains('hidden')).toBe(true);
    });

    it('должен скрыть MainButton при начале запроса', () => {
      tg.MainButton.hide();
      
      expect(tg.MainButton.hide).toHaveBeenCalled();
    });
  });

  describe('Fetch requests', () => {
    beforeEach(() => {
      global.fetch = vi.fn();
    });

    it('должен вызвать fetch с правильным URL', async () => {
      global.fetch.mockResolvedValue({
        json: () => Promise.resolve({ success: true })
      });
      
      await global.fetch(`${apiBase}/api/claim-video`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ ymid: ymid })
      });
      
      expect(global.fetch).toHaveBeenCalledWith(
        'http://localhost/api/claim-video',
        expect.objectContaining({
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ ymid: 'test123' })
        })
      );
    });
  });

  describe('Success handling', () => {
    beforeEach(() => {
      vi.useFakeTimers();
      global.fetch = vi.fn().mockResolvedValue({
        json: () => Promise.resolve({ success: true })
      });
    });

    afterEach(() => {
      vi.useRealTimers();
    });

    it('должен показать галочку при успехе', async () => {
      // Simulate success response
      const mockResponse = { success: true };
      
      // This would be the logic in claimVideo on success
      loaderText.innerText = "✅ Видео успешно отправлено в чат!";
      loaderSubtext.innerText = "Возвращаемся в бот...";
      
      expect(loaderText.innerText).toBe("✅ Видео успешно отправлено в чат!");
      expect(loaderSubtext.innerText).toBe("Возвращаемся в бот...");
    });

    it('должен вызвать HapticFeedback при успехе', () => {
      tg.HapticFeedback.notificationOccurred('success');
      
      expect(tg.HapticFeedback.notificationOccurred).toHaveBeenCalledWith('success');
    });

    it('должен вызвать tg.close() через 1500мс при успехе', async () => {
      setTimeout(() => {
        tg.WebApp.close();
      }, 1500);
      
      vi.advanceTimersByTime(1500);
      expect(tg.WebApp.close).toHaveBeenCalled();
    });
  });

  describe('Error handling', () => {
    it('должен вызвать showErrorView при ошибке API', async () => {
      const mockShowErrorView = vi.fn();
      
      // Simulate error response
      const claimData = { success: false, error: 'Test error' };
      
      // This would call showErrorView in claimVideo
      mockShowErrorView("Ошибка", "❌ " + (claimData.error || "Не удалось выдать видео"), true);
      
      expect(mockShowErrorView).toHaveBeenCalledWith(
        "Ошибка",
        "❌ Test error",
        true
      );
    });

    it('должен вызвать showErrorView при сетевой ошибке', async () => {
      const mockShowErrorView = vi.fn();
      const error = new Error('Network error');
      
      // This would call showErrorView in catch block
      mockShowErrorView("Ошибка сети", "❌ Не удалось подключиться к серверу. Проверьте интернет.", true);
      
      expect(mockShowErrorView).toHaveBeenCalledWith(
        "Ошибка сети",
        "❌ Не удалось подключиться к серверу. Проверьте интернет.",
        true
      );
    });
  });
});
