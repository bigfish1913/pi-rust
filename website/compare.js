import { initI18n } from './i18n.js';

const $ = (selector, scope = document) => scope.querySelector(selector);
const $$ = (selector, scope = document) => [...scope.querySelectorAll(selector)];

function showToast(message) {
  const toast = $('[data-toast]'); toast.textContent = message; toast.classList.add('is-visible');
  clearTimeout(showToast.timer); showToast.timer = setTimeout(() => toast.classList.remove('is-visible'), 2200);
}

function bindMenu() {
  const toggle = $('[data-menu-toggle]'); const menu = $('[data-mobile-menu]');
  if (!toggle || !menu) return;
  toggle.addEventListener('click', () => { const open = menu.classList.toggle('is-open'); toggle.setAttribute('aria-expanded', String(open)); });
  $$('a', menu).forEach((link) => link.addEventListener('click', () => { menu.classList.remove('is-open'); toggle.setAttribute('aria-expanded', 'false'); }));
}

function bindScrollEffects() {
  const header = $('[data-sticky-header]');
  if (!header) return;
  const marker = document.createElement('div'); marker.className = 'scroll-marker'; document.body.prepend(marker);
  new IntersectionObserver(([entry]) => header.classList.toggle('is-scrolled', !entry.isIntersecting), { threshold: 0 }).observe(marker);
}

async function init() {
  await initI18n();
  bindMenu();
  bindScrollEffects();
}

init().catch((error) => { console.error(error); showToast('Failed to load page data.'); });
