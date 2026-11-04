import { initI18n } from './i18n.js?v=20261104-community';

function refreshCommunityImages(now = new Date()) {
  // Use the same UTC+8 calendar day as the README refresh, regardless of visitor timezone.
  const day = new Date(now.getTime() + 8 * 60 * 60 * 1000).toISOString().slice(0, 10);
  const url = `https://assets.bigfisharc.cn/images/support/weixin.png?date=${day}`;
  document.querySelectorAll('[data-wechat-group]').forEach((element) => {
    const attribute = element.tagName === 'IMG' ? 'src' : 'href';
    if (element.getAttribute(attribute) !== url) element.setAttribute(attribute, url);
  });
}

// Refresh before loading translations so QR cache keys do not depend on that request.
refreshCommunityImages();
setInterval(refreshCommunityImages, 60 * 1000);
window.addEventListener('pageshow', () => refreshCommunityImages());
document.addEventListener('visibilitychange', () => {
  if (!document.hidden) refreshCommunityImages();
});

await initI18n();

const toggle = document.querySelector('[data-menu-toggle]');
const menu = document.querySelector('[data-mobile-menu]');
toggle.addEventListener('click', () => {
  const open = menu.classList.toggle('is-open');
  toggle.setAttribute('aria-expanded', String(open));
});
menu.querySelectorAll('a').forEach((link) => link.addEventListener('click', () => {
  menu.classList.remove('is-open');
  toggle.setAttribute('aria-expanded', 'false');
}));
