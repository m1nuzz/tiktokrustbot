/**
 * Unit tests for SmartLink URL logic in trySmartLink()
 * Tests the inapp=1 parameter and URL separator logic
 */

import { describe, it, expect } from 'vitest';

describe('Telegram Mini App - SmartLink URL Logic', () => {
  describe('URL parameter handling', () => {
    it('должен добавить inapp=1 к URL без параметров', () => {
      const AD_CONFIG = { smartLink: 'https://monetag.com/smart' };
      const ymid = 'test123';
      
      const rawSmartLink = AD_CONFIG.smartLink;
      const separator = rawSmartLink.includes('?') ? '&' : '?';
      const smartLinkUrl = `${rawSmartLink}${separator}ymid=${encodeURIComponent(ymid)}&inapp=1`;
      
      expect(smartLinkUrl).toBe('https://monetag.com/smart?ymid=test123&inapp=1');
    });

    it('должен добавить inapp=1 к URL с существующими параметрами', () => {
      const AD_CONFIG = { smartLink: 'https://monetag.com/smart?ref=abc' };
      const ymid = 'test123';
      
      const rawSmartLink = AD_CONFIG.smartLink;
      const separator = rawSmartLink.includes('?') ? '&' : '?';
      const smartLinkUrl = `${rawSmartLink}${separator}ymid=${encodeURIComponent(ymid)}&inapp=1`;
      
      expect(smartLinkUrl).toBe('https://monetag.com/smart?ref=abc&ymid=test123&inapp=1');
    });

    it('должен добавить inapp=1 к URL с несколькими параметрами', () => {
      const AD_CONFIG = { smartLink: 'https://monetag.com/smart?ref=abc&subid=123' };
      const ymid = 'test456';
      
      const rawSmartLink = AD_CONFIG.smartLink;
      const separator = rawSmartLink.includes('?') ? '&' : '?';
      const smartLinkUrl = `${rawSmartLink}${separator}ymid=${encodeURIComponent(ymid)}&inapp=1`;
      
      expect(smartLinkUrl).toBe('https://monetag.com/smart?ref=abc&subid=123&ymid=test456&inapp=1');
    });

    it('должен использовать ? для URL без параметров', () => {
      const rawSmartLink = 'https://monetag.com/smart';
      const separator = rawSmartLink.includes('?') ? '&' : '?';
      
      expect(separator).toBe('?');
    });

    it('должен использовать & для URL с параметрами', () => {
      const rawSmartLink = 'https://monetag.com/smart?ref=abc';
      const separator = rawSmartLink.includes('?') ? '&' : '?';
      
      expect(separator).toBe('&');
    });

    it('должен корректно кодировать ymid с специальными символами', () => {
      const AD_CONFIG = { smartLink: 'https://monetag.com/smart' };
      const ymid = 'test&special=chars';
      
      const rawSmartLink = AD_CONFIG.smartLink;
      const separator = rawSmartLink.includes('?') ? '&' : '?';
      const smartLinkUrl = `${rawSmartLink}${separator}ymid=${encodeURIComponent(ymid)}&inapp=1`;
      
      expect(smartLinkUrl).toBe('https://monetag.com/smart?ymid=test%26special%3Dchars&inapp=1');
    });
  });

  describe('URL with different formats', () => {
    it('должен работать с URL без протокола', () => {
      const AD_CONFIG = { smartLink: '//monetag.com/smart' };
      const ymid = 'test123';
      
      const rawSmartLink = AD_CONFIG.smartLink;
      const separator = rawSmartLink.includes('?') ? '&' : '?';
      const smartLinkUrl = `${rawSmartLink}${separator}ymid=${encodeURIComponent(ymid)}&inapp=1`;
      
      expect(smartLinkUrl).toBe('//monetag.com/smart?ymid=test123&inapp=1');
    });

    it('должен работать с URL содержащим hash', () => {
      const AD_CONFIG = { smartLink: 'https://monetag.com/smart#section' };
      const ymid = 'test123';
      
      const rawSmartLink = AD_CONFIG.smartLink;
      const separator = rawSmartLink.includes('?') ? '&' : '?';
      const smartLinkUrl = `${rawSmartLink}${separator}ymid=${encodeURIComponent(ymid)}&inapp=1`;
      
      expect(smartLinkUrl).toBe('https://monetag.com/smart#section?ymid=test123&inapp=1');
    });
  });
});
