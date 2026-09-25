import { initI18n, getLocale, onLocaleChange, t } from './i18n.js';

const $ = (sel, scope = document) => scope.querySelector(sel);
const $$ = (sel, scope = document) => [...scope.querySelectorAll(sel)];

const GUIDES = [
  { id: 'user-guide', file: 'user-guide.md', title: { zh: '使用手册', en: 'User Guide' } },
  { id: 'extension-authoring', file: 'extension-authoring.md', title: { zh: '扩展开发', en: 'Extension Authoring' } },
  { id: 'agent-project', file: 'agent-project.md', title: { zh: 'Agent 项目结构', en: 'Agent Project Structure' } },
];

let currentGuide = null;
let markdownCache = {};

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

function renderMarkdown(text) {
  // Code blocks
  text = text.replace(/```(\w*)\n([\s\S]*?)```/g, (_, lang, code) => {
    return `<pre><code class="language-${lang}">${escapeHtml(code.trim())}</code></pre>`;
  });

  // Tables
  text = text.replace(/^(\|.+\|)\n(\|[-| :]+\|)\n((?:\|.+\|\n?)*)/gm, (match, headerRow, separatorRow, bodyRows) => {
    const headers = headerRow.split('|').filter(c => c.trim()).map(c => `<th>${c.trim()}</th>`).join('');
    const rows = bodyRows.trim().split('\n').map(row => {
      const cells = row.split('|').filter(c => c.trim()).map(c => `<td>${c.trim()}</td>`).join('');
      return `<tr>${cells}</tr>`;
    }).join('');
    return `<table><thead><tr>${headers}</tr></thead><tbody>${rows}</tbody></table>`;
  });

  // Headings
  text = text.replace(/^#### (.+)$/gm, '<h4 id="$1">$1</h4>');
  text = text.replace(/^### (.+)$/gm, '<h3 id="$1">$1</h3>');
  text = text.replace(/^## (.+)$/gm, '<h2 id="$1">$1</h2>');
  text = text.replace(/^# (.+)$/gm, '<h1 id="$1">$1</h1>');

  // Blockquotes
  text = text.replace(/^> (.+)$/gm, '<blockquote><p>$1</p></blockquote>');

  // Unordered lists
  text = text.replace(/^- (.+)$/gm, '<li>$1</li>');
  text = text.replace(/(<li>.*<\/li>\n?)+/g, '<ul>$&</ul>');

  // Ordered lists
  text = text.replace(/^\d+\. (.+)$/gm, '<li>$1</li>');

  // Bold
  text = text.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');

  // Inline code
  text = text.replace(/`([^`]+)`/g, '<code>$1</code>');

  // Links
  text = text.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noreferrer">$1</a>');

  // Paragraphs
  text = text.replace(/\n\n/g, '</p><p>');
  text = text.replace(/^/, '<p>');
  text = text.replace(/$/, '</p>');

  // Clean up empty paragraphs and fix nesting
  text = text.replace(/<p><\/p>/g, '');
  text = text.replace(/<p>(<h[1234]>)/g, '$1');
  text = text.replace(/(<\/h[1234]>)<\/p>/g, '$1');
  text = text.replace(/<p>(<pre>)/g, '$1');
  text = text.replace(/(<\/pre>)<\/p>/g, '$1');
  text = text.replace(/<p>(<ul>)/g, '$1');
  text = text.replace(/(<\/ul>)<\/p>/g, '$1');
  text = text.replace(/<p>(<table>)/g, '$1');
  text = text.replace(/(<\/table>)<\/p>/g, '$1');
  text = text.replace(/<p>(<blockquote>)/g, '$1');
  text = text.replace(/(<\/blockquote>)<\/p>/g, '$1');

  return text;
}

function extractHeadings(markdown) {
  const headings = [];
  const lines = markdown.split('\n');
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const h2Match = line.match(/^## (.+)$/);
    const h3Match = line.match(/^### (.+)$/);
    if (h2Match) {
      headings.push({ level: 2, text: h2Match[1], id: h2Match[1] });
    } else if (h3Match) {
      headings.push({ level: 3, text: h3Match[1], id: h3Match[1] });
    }
  }
  return headings;
}

function renderToc(headings) {
  const nav = $('[data-guide-toc]');
  if (!headings.length) {
    nav.innerHTML = '';
    return;
  }
  nav.innerHTML = headings.map(h => 
    `<a href="#${h.id}" class="level-${h.level}">${h.text}</a>`
  ).join('');

  // Bind click events for smooth scrolling
  $$('a', nav).forEach(link => {
    link.addEventListener('click', (e) => {
      e.preventDefault();
      const id = link.getAttribute('href').slice(1);
      const target = document.getElementById(id);
      if (target) {
        target.scrollIntoView({ behavior: 'smooth', block: 'start' });
        history.replaceState(null, '', `#${id}`);
      }
    });
  });
}

function updateTocHighlight() {
  const headings = $$('[data-guide-body] h2, [data-guide-body] h3');
  const tocLinks = $$('[data-guide-toc] a');
  
  if (!headings.length || !tocLinks.length) return;

  let activeId = null;
  const scrollTop = window.scrollY + 150;

  for (const heading of headings) {
    if (heading.offsetTop <= scrollTop) {
      activeId = heading.id;
    }
  }

  tocLinks.forEach(link => {
    const href = link.getAttribute('href').slice(1);
    link.classList.toggle('is-active', href === activeId);
  });
}

async function loadGuide(guideId) {
  const guide = GUIDES.find(g => g.id === guideId);
  if (!guide) return;

  currentGuide = guide;
  const locale = getLocale();

  // Update title
  $('[data-guide-title]').textContent = guide.title[locale] || guide.title.zh;
  document.title = `${guide.title[locale] || guide.title.zh} · rpi`;

  // Update selector
  const selector = $('[data-guide-selector]');
  selector.innerHTML = GUIDES.map(g => 
    `<button data-guide-id="${g.id}" class="${g.id === guideId ? 'is-active' : ''}">${g.title[locale] || g.title.zh}</button>`
  ).join('');

  $$('[data-guide-id]', selector).forEach(btn => {
    btn.addEventListener('click', () => {
      loadGuide(btn.dataset.guideId);
      history.replaceState(null, '', `?doc=${btn.dataset.guideId}`);
    });
  });

  // Load markdown
  const body = $('[data-guide-body]');
  body.innerHTML = '<div class="guide-loading">加载中...</div>';

  let markdown = markdownCache[guide.file];
  if (!markdown) {
    try {
      const response = await fetch(`./${guide.file}`);
      markdown = await response.text();
      markdownCache[guide.file] = markdown;
    } catch (err) {
      body.innerHTML = '<div class="guide-error">加载失败，请稍后重试。</div>';
      return;
    }
  }

  // Render
  const html = renderMarkdown(markdown);
  body.innerHTML = html;

  // Build TOC
  const headings = extractHeadings(markdown);
  renderToc(headings);

  // Scroll to top
  window.scrollTo({ top: 0, behavior: 'smooth' });

  // Setup scroll spy
  window.addEventListener('scroll', updateTocHighlight, { passive: true });
  updateTocHighlight();
}

function getGuideFromUrl() {
  const params = new URLSearchParams(window.location.search);
  return params.get('doc') || 'user-guide';
}

async function init() {
  await initI18n();
  const guideId = getGuideFromUrl();
  await loadGuide(guideId);

  onLocaleChange(() => {
    if (currentGuide) {
      loadGuide(currentGuide.id);
    }
  });
}

init().catch(err => {
  console.error('Guide init failed:', err);
  $('[data-guide-body]').innerHTML = '<div class="guide-error">加载失败。</div>';
});
