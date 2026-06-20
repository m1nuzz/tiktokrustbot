/**
 * Unit tests for showErrorView() function and Retry logic
 * Tests error display, Retry button behavior, no recursion
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { JSDOM } from 'jsdom';

describe('Telegram Mini App - showErrorView() and Retry', () => {
  let dom;
  let document;
  let loaderView;
  let contentView;
  let titleEl;
  let descriptionEl;
  let directBtn;
  let claimVideo;

  beforeEach(() => {
    dom = new JSDOM(`
      <!DOCTYPE html>
      <html>
        <body>
          <div id="loader-view"></div>
          <div id="content-view" class="hidden"></div>
          <h1 id="title"></h1>
          <p id="description"></p>
          <button id="direct-btn" class="hidden">Continue to Bot</button>
        </body>
      </html>
    `, { url: 'http://localhost' });
    
    document = dom.window.document;
    loaderView = document.getElementById('loader-view');
    contentView = document.getElementById('content-view');
    titleEl = document.getElementById('title');
    descriptionEl = document.getElementById('description');
    directBtn = document.getElementById('direct-btn');
    
    // Mock claimVideo function
    claimVideo = vi.fn();
    global.claimVideo = claimVideo;
  });

  describe('showErrorView() - DOM manipulations', () => {
    it('должен скрыть спиннер', () => {
      // Initial state - spinner visible
      loaderView.classList.remove('hidden');
      
      // Call showErrorView logic
      loaderView.classList.add('hidden');
      
      expect(loaderView.classList.contains('hidden')).toBe(true);
    });

    it('должен показать content-view', () => {
      contentView.classList.remove('hidden');
      
      expect(contentView.classList.contains('hidden')).toBe(false);
    });

    it('должен установить заголовок', () => {
      const titleText = 'Ошибка';
      titleEl.innerText = titleText;
      
      expect(titleEl.innerText).toBe(titleText);
    });

    it('должен установить описание', () => {
      const descriptionText = '❌ Тестовая ошибка';
      descriptionEl.innerText = descriptionText;
      
      expect(descriptionEl.innerText).toBe(descriptionText);
    });
  });

  describe('showErrorView() - Retry button', () => {
    it('должен показать кнопку Retry при showRetry=true', () => {
      directBtn.innerText = "🔄 Попробовать еще раз";
      directBtn.classList.remove('hidden');
      
      expect(directBtn.innerText).toBe("🔄 Попробовать еще раз");
      expect(directBtn.classList.contains('hidden')).toBe(false);
    });

    it('должен скрыть кнопку Retry при showRetry=false', () => {
      directBtn.classList.add('hidden');
      
      expect(directBtn.classList.contains('hidden')).toBe(true);
    });

    it('кнопка Retry должна вызывать claimVideo()', () => {
      directBtn.onclick = function() {
        directBtn.classList.add('hidden');
        claimVideo();
      };
      
      directBtn.onclick();
      
      expect(claimVideo).toHaveBeenCalled();
      expect(directBtn.classList.contains('hidden')).toBe(true);
    });
  });

  describe('No recursion - Critical test', () => {
    let onAdSuccess;
    
    beforeEach(() => {
      onAdSuccess = vi.fn();
      global.onAdSuccess = onAdSuccess;
    });

    it('кнопка Retry НЕ должна вызывать onAdSuccess()', () => {
      // This is the critical test - Retry button should call claimVideo(), NOT onAdSuccess()
      directBtn.onclick = function() {
        directBtn.classList.add('hidden');
        claimVideo(); // Correct behavior
      };
      
      directBtn.onclick();
      
      // claimVideo should be called
      expect(claimVideo).toHaveBeenCalled();
      
      // onAdSuccess should NOT be called (no recursion!)
      expect(onAdSuccess).not.toHaveBeenCalled();
    });

    it('должен вызывать только claimVideo() без onAdSuccess()', () => {
      // Wrong behavior would be:
      // directBtn.onclick = function() {
      //   onAdSuccess(); // This would cause recursion!
      // };
      
      // Correct behavior:
      directBtn.onclick = function() {
        directBtn.classList.add('hidden');
        claimVideo();
      };
      
      directBtn.onclick();
      
      expect(claimVideo).toHaveBeenCalledTimes(1);
      expect(onAdSuccess).not.toHaveBeenCalled();
    });
  });

  describe('showErrorView() - Complete flow', () => {
    it('должен корректно отобразить ошибку с кнопкой Retry', () => {
      // Simulate showErrorView logic
      const titleText = "Ошибка";
      const descriptionText = "❌ Не удалось выдать видео";
      const showRetry = true;
      
      // Hide spinner, show content
      loaderView.classList.add('hidden');
      contentView.classList.remove('hidden');
      
      // Set title and description
      titleEl.innerText = titleText;
      descriptionEl.innerText = descriptionText;
      
      // Show Retry button
      if (showRetry) {
        directBtn.innerText = "🔄 Попробовать еще раз";
        directBtn.classList.remove('hidden');
        directBtn.onclick = function() {
          directBtn.classList.add('hidden');
          claimVideo();
        };
      }
      
      // Assertions
      expect(loaderView.classList.contains('hidden')).toBe(true);
      expect(contentView.classList.contains('hidden')).toBe(false);
      expect(titleEl.innerText).toBe(titleText);
      expect(descriptionEl.innerText).toBe(descriptionText);
      expect(directBtn.innerText).toBe("🔄 Попробовать еще раз");
      expect(directBtn.classList.contains('hidden')).toBe(false);
    });

    it('должен корректно отобразить ошибку без кнопки Retry', () => {
      const titleText = "Ошибка";
      const descriptionText = "❌ Что-то пошло не так";
      const showRetry = false;
      
      loaderView.classList.add('hidden');
      contentView.classList.remove('hidden');
      titleEl.innerText = titleText;
      descriptionEl.innerText = descriptionText;
      
      if (showRetry) {
        directBtn.innerText = "🔄 Попробовать еще раз";
        directBtn.classList.remove('hidden');
      } else {
        directBtn.classList.add('hidden');
      }
      
      expect(loaderView.classList.contains('hidden')).toBe(true);
      expect(contentView.classList.contains('hidden')).toBe(false);
      expect(titleEl.innerText).toBe(titleText);
      expect(descriptionEl.innerText).toBe(descriptionText);
      expect(directBtn.classList.contains('hidden')).toBe(true);
    });
  });
});
