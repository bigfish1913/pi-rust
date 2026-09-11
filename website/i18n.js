const LOCALE_KEY = 'rpi-locale';
const FALLBACK_LOCALE = 'zh';
const listeners = new Set();
let dictionary = null;
let locale = detectLocale();

function detectLocale() {
  const queryLocale = new URLSearchParams(window.location.search).get('lang');
  if (queryLocale === 'en' || queryLocale === 'zh') return queryLocale;
  let storedLocale = null;
  try { storedLocale = window.localStorage.getItem(LOCALE_KEY); } catch { /* Storage may be disabled in private browsing. */ }
  if (storedLocale === 'en' || storedLocale === 'zh') return storedLocale;
  return (navigator.language || '').toLowerCase().startsWith('zh') ? 'zh' : 'en';
}

export function getLocale() { return locale; }

export function t(key, variables = {}) {
  const value = dictionary?.[locale]?.ui?.[key] ?? dictionary?.[FALLBACK_LOCALE]?.ui?.[key] ?? key;
  return Object.entries(variables).reduce((result, [name, replacement]) => result.replaceAll(`{${name}}`, String(replacement)), value);
}

function mergeLocaleData(source, overrides) {
  if (!overrides) return source;
  const result = structuredClone(source);
  const merge = (target, patch) => Object.entries(patch).forEach(([key, value]) => {
    if (Array.isArray(value) && Array.isArray(target[key])) {
      target[key] = value.map((item, index) => {
        const original = item?.id ? target[key].find((candidate) => candidate.id === item.id) : target[key][index];
        return original && item && typeof item === 'object' ? { ...original, ...item } : item;
      });
    } else if (value && typeof value === 'object' && !Array.isArray(value) && target[key]) merge(target[key], value);
    else target[key] = value;
  });
  merge(result, overrides);
  return result;
}

export function localizeData(source, page) {
  if (locale === FALLBACK_LOCALE) return source;
  const overrides = dictionary?.[locale]?.[page];
  if (!overrides) return source;
  const localized = mergeLocaleData(source, overrides);
  if (page === 'packages') {
    localized.forEach((item) => {
      if (dictionary[locale].packages[item.name]) item.description = dictionary[locale].packages[item.name];
    });
  }
  if (page === 'docs') {
    Object.entries(overrides.modes || {}).forEach(([modeKey, modePatch]) => {
      const mode = localized.modes[modeKey];
      if (!mode) return;
      Object.entries(modePatch.sections || {}).forEach(([sectionId, sectionPatch]) => {
        const section = mode.sections.find((item) => item.id === sectionId);
        if (section) {
          const patch = { ...sectionPatch };
          if (Array.isArray(patch.links) && patch.links.every((link) => typeof link === 'string')) {
            patch.links = patch.links.map((label, index) => ({ ...(section.links[index] || {}), label }));
          }
          Object.assign(section, patch);
        }
      });
    });
  }
  return localized;
}

export function applyUi() {
  document.documentElement.lang = locale === 'en' ? 'en' : 'zh-CN';
  document.querySelectorAll('[data-i18n]').forEach((element) => { element.textContent = t(element.dataset.i18n); });
  document.querySelectorAll('[data-i18n-html]').forEach((element) => { element.innerHTML = t(element.dataset.i18nHtml).replaceAll('\n', '<br />'); });
  document.querySelectorAll('[data-i18n-attr]').forEach((element) => {
    element.dataset.i18nAttr.split(';').forEach((mapping) => {
      const [attribute, key] = mapping.split('|');
      element.setAttribute(attribute, t(key));
    });
  });
  document.querySelectorAll('[data-language-toggle]').forEach((button) => {
    button.textContent = locale === 'en' ? '中' : 'EN';
    button.setAttribute('aria-label', locale === 'en' ? '切换为中文' : 'Switch to English');
    button.title = locale === 'en' ? '切换为中文' : 'Switch to English';
  });
  document.documentElement.dataset.i18nReady = 'true';
}

export function onLocaleChange(listener) { listeners.add(listener); return () => listeners.delete(listener); }

function persistLocale(nextLocale) {
  try { window.localStorage.setItem(LOCALE_KEY, nextLocale); } catch { /* URL state still keeps navigation consistent. */ }
  const url = new URL(window.location.href);
  url.searchParams.set('lang', nextLocale);
  window.history.replaceState(null, '', `${url.pathname}${url.search}${url.hash}`);
}

function syncLocaleLinks() {
  document.querySelectorAll('a[href]').forEach((link) => {
    const rawHref = link.getAttribute('href');
    if (!rawHref || rawHref.startsWith('#') || rawHref.startsWith('mailto:') || rawHref.startsWith('javascript:')) return;
    let target;
    try { target = new URL(rawHref, window.location.href); } catch { return; }
    if (target.origin !== window.location.origin || (!target.pathname.endsWith('.html') && target.pathname !== '/' && !target.pathname.endsWith('/'))) return;
    target.searchParams.set('lang', locale);
    link.setAttribute('href', `${target.pathname}${target.search}${target.hash}`);
  });
}

export async function initI18n() {
  if (!dictionary) {
    const response = await fetch('./data/locales.json');
    dictionary = await response.json();
  }
  applyUi();
  syncLocaleLinks();
  document.querySelectorAll('[data-language-toggle]').forEach((button) => {
    if (button.dataset.i18nBound) return;
    button.dataset.i18nBound = 'true';
    button.addEventListener('click', () => {
      locale = locale === 'en' ? 'zh' : 'en';
      persistLocale(locale);
      applyUi();
      syncLocaleLinks();
      listeners.forEach((listener) => listener(locale));
    });
  });
  return locale;
}
