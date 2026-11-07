"""Website checks. Set CADDY_BIN to also exercise the real Caddy configuration."""

import contextlib
import importlib.util
import io
import json
import os
import re
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('verify_website', ROOT / 'scripts/verify-website.py')
website = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(website)


class VerifierTests(unittest.TestCase):
    def check_response(self, response, **kwargs):
        with patch.object(website, 'fetch', return_value=response), contextlib.redirect_stdout(io.StringIO()):
            website.check('https://example.test', '/', 200, website.CONTENT_CACHE, **kwargs)

    def test_cache_header_order_is_irrelevant(self):
        self.check_response((200, {'Cache-Control': 'must-revalidate, s-maxage=60, public, max-age=0'}, b'ok'))

    def test_missing_cache_header_fails(self):
        with self.assertRaisesRegex(AssertionError, 'Cache-Control'):
            self.check_response((200, {}, b'ok'))

    def test_wrong_status_fails(self):
        with self.assertRaisesRegex(AssertionError, 'expected HTTP 200'):
            self.check_response((404, {'Cache-Control': 'no-store'}, b''))

    def test_wrong_content_fails(self):
        with self.assertRaisesRegex(AssertionError, 'expected content'):
            self.check_response((200, {'Cache-Control': website.CONTENT_CACHE}, b'wrong'), marker=b'rpi-cli')

    def test_wrong_redirect_fails(self):
        with self.assertRaisesRegex(AssertionError, 'unexpected redirect'):
            self.check_response((200, {'Cache-Control': website.CONTENT_CACHE}, b''), location='/guide.html')

    def test_invalid_base_url_fails(self):
        for url in ('file:///tmp/site', 'https://example.test/?lang=en', 'https://example.test/#top'):
            with self.subTest(url=url), self.assertRaises(ValueError):
                website.verify(url)


class CommunityTests(unittest.TestCase):
    def test_donation_images_use_shared_assets(self):
        html = (ROOT / 'website/support.html').read_text(encoding='utf-8')
        for name in ('wechat.png', 'alipay.jpg', 'binance.png'):
            url = f'https://assets.bigfisharc.cn/images/support/{name}'
            self.assertIn(f'src="{url}"', html)
            self.assertEqual(html.count(f'href="{url}"'), 2)
        self.assertNotIn('./images/support/', html)

    def test_homepage_join_links_and_community_target(self):
        home = (ROOT / 'website/index.html').read_text(encoding='utf-8')
        support = (ROOT / 'website/support.html').read_text(encoding='utf-8')
        self.assertIn('href="./support.html#community"', home)
        self.assertIn('id="community" aria-labelledby="community-title"', support)
        for html in (home, support):
            self.assertIn('href="https://t.me/+7YfN4TZPP7Y0M2Q1"', html)
        self.assertEqual(support.count('data-wechat-group'), 3)

    def test_telegram_qr_uses_shared_asset_and_links_to_group(self):
        html = (ROOT / 'website/support.html').read_text(encoding='utf-8')
        self.assertRegex(html, r'<a class="community-qr" href="https://t\.me/\+7YfN4TZPP7Y0M2Q1"[^>]*><img src="https://assets\.bigfisharc\.cn/images/support/tg\.png"[^>]*alt="[^"]+"')

    def test_community_labels_are_translated_in_both_languages(self):
        locales = json.loads((ROOT / 'website/data/locales.json').read_text(encoding='utf-8'))
        for name in ('index.html', 'support.html'):
            html = (ROOT / 'website' / name).read_text(encoding='utf-8')
            keys = re.findall(r'data-i18n="([^"]+)"', html)
            keys += re.findall(r'\|((?:community|support)\.[^";]+)', html)
            for key in keys:
                for language in ('zh', 'en'):
                    with self.subTest(page=name, key=key, language=language):
                        self.assertTrue(locales[language]['ui'][key])

    @unittest.skipUnless(shutil.which('node'), 'set up Node.js to test browser date refresh')
    def test_wechat_cache_key_and_refresh_events(self):
        # Exercise the real page script without network, a browser or npm dependencies.
        script = r'''
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync('website/support.js', 'utf8').replace(/^import .*;\r?\n/, '');
const elements = ['A', 'IMG', 'A'].map(tagName => ({
  tagName, attributes: {}, writes: 0,
  getAttribute(key) { return this.attributes[key]; },
  setAttribute(key, value) { this.attributes[key] = value; this.writes++; }
}));
const pageEvents = {}, documentEvents = {}, intervals = [];
const menu = { addEventListener() {}, querySelectorAll() { return []; } };
const document = {
  hidden: false,
  querySelectorAll(selector) { assert.equal(selector, '[data-wechat-group]'); return elements; },
  querySelector() { return menu; },
  addEventListener(event, callback) { documentEvents[event] = callback; }
};
const context = {
  document, initI18n: async () => {},
  window: { addEventListener(event, callback) { pageEvents[event] = callback; } },
  setInterval(callback, delay) { intervals.push({ callback, delay }); }
};
(async () => {
  await vm.runInNewContext('(async () => {' + source + '\n globalThis.refresh = refreshCommunityImages; })()', context);
  const url = day => `https://assets.bigfisharc.cn/images/support/weixin.png?date=${day}`;
  const verify = day => elements.forEach(element => {
    assert.equal(element.attributes[element.tagName === 'IMG' ? 'src' : 'href'], url(day));
  });
  context.refresh(new Date('2026-10-31T15:59:59Z'));
  verify('2026-10-31');
  context.refresh(new Date('2026-10-31T16:00:00Z'));
  verify('2026-11-01');
  const writes = elements.map(element => element.writes);
  context.refresh(new Date('2026-11-01T03:00:00Z'));
  assert.deepEqual(elements.map(element => element.writes), writes);
  context.refresh(new Date('2028-02-28T16:00:00Z'));
  verify('2028-02-29');
  assert.equal(intervals[0].delay, 60000);
  assert.equal(intervals[0].callback, context.refresh);
  pageEvents.pageshow();
  documentEvents.visibilitychange();
  document.hidden = true;
  const hiddenWrites = elements.map(element => element.writes);
  documentEvents.visibilitychange();
  assert.deepEqual(elements.map(element => element.writes), hiddenWrites);
})().catch(error => { console.error(error); process.exitCode = 1; });
'''
        subprocess.run([shutil.which('node'), '-e', script], cwd=ROOT, check=True, timeout=30)


@unittest.skipUnless(os.environ.get('CADDY_BIN'), 'set CADDY_BIN to run HTTP integration tests')
class CaddyIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cleanup = contextlib.ExitStack()
        try:
            cls.start_server()
        except Exception:
            cls.cleanup.close()
            raise

    @classmethod
    def tearDownClass(cls):
        cls.cleanup.close()

    @classmethod
    def start_server(cls):
        temp = cls.cleanup.enter_context(tempfile.TemporaryDirectory(prefix='rpi-website-test-'))
        directory = Path(temp)
        site = directory / 'website'
        shutil.copytree(ROOT / 'website', site, ignore=shutil.ignore_patterns('.rpi', '.deploy', '.git'))
        # Prove that the server denies real files, not just paths that happen to be absent.
        for path in ('.rpi/todo.json', '.deploy/website.tar.gz', '.git/config', 'nested/.rpi/session.json', '.RPI/todo.json'):
            target = site / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text('PRIVATE_SENTINEL', encoding='utf-8')
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        cls.base_url = f'http://127.0.0.1:{port}'
        config = (ROOT / 'deploy/rpi.caddy').read_text(encoding='utf-8')
        config = config.split('\nhttp://rpi.laofu.online {')[0]
        config = config.replace('rpi.laofu.online {', f'http://127.0.0.1:{port} {{', 1)
        config = config.replace('root * /var/www/rpi', f'root * "{site.as_posix()}"')
        config = config.replace('    tls {\n        issuer acme\n    }', '')
        config = '{\n    admin off\n    auto_https off\n    persist_config off\n}\n' + config
        config_path = directory / 'Caddyfile'
        config_path.write_text(config, encoding='utf-8')
        command = [os.environ['CADDY_BIN'], 'run', '--config', str(config_path), '--adapter', 'caddyfile']
        log = cls.cleanup.enter_context((directory / 'caddy.log').open('w+', encoding='utf-8'))
        cls.process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
        cls.cleanup.callback(cls.stop_server)
        for _ in range(100):
            if cls.process.poll() is not None:
                log.seek(0)
                raise RuntimeError(log.read())
            try:
                website.fetch(cls.base_url, '/')
                break
            except OSError:
                time.sleep(0.05)
        else:
            raise RuntimeError('Caddy did not start within 5 seconds')

    @classmethod
    def stop_server(cls):
        cls.process.terminate()
        try:
            cls.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            cls.process.kill()
            cls.process.wait(timeout=5)

    def test_complete_http_contract(self):
        with contextlib.redirect_stdout(io.StringIO()):
            website.verify(self.base_url)

    def test_private_contents_are_never_served(self):
        for path in ('/.rpi/todo.json', '/nested/.rpi/session.json', '/.RPI/todo.json', '/%2erpi/todo.json'):
            with self.subTest(path=path):
                status, headers, body = website.fetch(self.base_url, path)
                self.assertEqual(status, 404)
                self.assertEqual(headers.get('Cache-Control'), 'no-store')
                self.assertNotIn(b'PRIVATE_SENTINEL', body)

    def test_all_guide_redirects_are_uncacheable(self):
        for doc in ('user-guide', 'extension-authoring', 'agent-project'):
            with self.subTest(doc=doc), contextlib.redirect_stdout(io.StringIO()):
                website.check(self.base_url, f'/{doc}.md', 302, 'no-store', location=f'/guide.html?doc={doc}')


if __name__ == '__main__':
    unittest.main()
