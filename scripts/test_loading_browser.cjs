// Real Chromium, loopback fixtures only. No Reddit requests or npm dependencies.
// Run with: node --test scripts/test_loading_browser.cjs (CHROME_BIN optional).
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const test = require('node:test');
const { setTimeout: delay } = require('node:timers/promises');

class CDP {
    static async connect(url) {
        const client = new CDP();
        client.socket = new WebSocket(url);
        client.pending = new Map();
        client.id = 0;
        client.socket.addEventListener('message', event => {
            const message = JSON.parse(event.data);
            const pending = client.pending.get(message.id);
            if (!pending) return;
            client.pending.delete(message.id);
            if (message.error) pending.reject(new Error(JSON.stringify(message.error)));
            else pending.resolve(message.result);
        });
        await new Promise((resolve, reject) => {
            const timer = setTimeout(() => { client.close(); reject(new Error('Browser connection timed out')); }, 5000);
            client.socket.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
            client.socket.addEventListener('error', () => { clearTimeout(timer); reject(new Error('Browser connection failed')); }, { once: true });
        });
        return client;
    }
    send(method, params = {}) {
        const id = ++this.id;
        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                this.pending.delete(id);
                reject(new Error(`Browser command timed out: ${method}`));
            }, 5000);
            this.pending.set(id, {
                resolve(value) { clearTimeout(timer); resolve(value); },
                reject(error) { clearTimeout(timer); reject(error); },
            });
            this.socket.send(JSON.stringify({ id, method, params }));
        });
    }
    async evaluate(expression) {
        const result = await this.send('Runtime.evaluate', { expression, returnByValue: true });
        assert.equal(result.exceptionDetails, undefined, JSON.stringify(result.exceptionDetails));
        return result.result.value;
    }
    close() { this.socket.close(); }
}

const root = path.resolve(__dirname, '..');
const csp = "default-src 'none'; script-src 'self' blob:; connect-src 'self'; base-uri 'none'; style-src 'self' 'unsafe-inline'; img-src 'self' data:";
const template = fs.readFileSync(path.join(root, 'templates/loading.html'), 'utf8');
function waiting(url) {
    // Use the actual template, shortening only the timer for this fixture.
    const meta = template.match(/<meta[^>]+http-equiv="refresh"[^>]+>/)[0]
        .replace('{{ refresh_after }}', '1')
        .replace('{{ url }}', url.replaceAll('&', '&amp;').replaceAll('"', '&quot;'));
    return `<!doctype html><html><head>${meta}</head><body>Still trying to load your page…</body></html>`;
}

async function waitUntil(check, message, milliseconds = 5000) {
    const deadline = Date.now() + milliseconds;
    while (Date.now() < deadline) {
        if (await check()) return;
        await delay(25);
    }
    assert.fail(message);
}

test('loading pages reload fragment URLs in a real browser', { timeout: 90000 }, async t => {
    const executable = process.env.CHROME_BIN || ['/usr/bin/google-chrome', '/usr/bin/chromium', '/usr/bin/chromium-browser'].find(fs.existsSync);
    assert.ok(executable, 'Install Chromium or set CHROME_BIN');
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'redlib-loading-'));
    const chrome = spawn(executable, ['--headless', '--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu', '--disable-background-networking', '--no-first-run', '--remote-debugging-port=0', `--user-data-dir=${directory}`, 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
    let browser;
    let stderr = '';
    chrome.stderr.on('data', chunk => { stderr += chunk.toString(); });
    chrome.on('error', error => { stderr += error.message; });
    t.after(async () => {
        if (browser) {
            // Let browser subprocesses flush and exit before deleting their
            // profile. Killing only the parent races their final writes.
            try { await browser.send('Browser.close'); } catch (_) { /* Fall back to process termination. */ }
            browser.close();
        }
        if (chrome.exitCode === null && chrome.signalCode === null) {
            const exited = once(chrome, 'exit');
            chrome.kill();
            await Promise.race([exited, delay(2000)]);
            if (chrome.exitCode === null && chrome.signalCode === null) {
                chrome.kill('SIGKILL');
                await exited;
            }
        }
        chrome.stderr.destroy();
        await fs.promises.rm(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    });
    try {
        await waitUntil(() => /DevTools listening on (ws:\/\/\S+)/.test(stderr), 'Browser did not start', 20000);
    } catch (error) {
        throw new Error(`${error.message}: ${executable}\n${stderr}`);
    }
    const browserUrl = stderr.match(/DevTools listening on (ws:\/\/\S+)/)[1];
    browser = await CDP.connect(browserUrl);
    const debugOrigin = browserUrl.replace(/^ws:/, 'http:').split('/devtools/')[0];
    const cases = new Map();
    const server = http.createServer((req, res) => {
        const scenario = cases.get(req.url);
        if (!scenario) { res.writeHead(404).end(); return; }
        scenario.requests.push({ url: req.url, cookie: req.headers.cookie, method: req.method });
        res.setHeader('Content-Security-Policy', csp);
        res.setHeader('Cache-Control', 'no-store');
        if (scenario.requests.length < 3) {
            res.writeHead(503, { 'Content-Type': 'text/html; charset=utf-8', 'Retry-After': '361', 'Set-Cookie': 'theme=dark; Path=/' });
            res.end(waiting(req.url));
        } else {
            res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
            res.end('<!doctype html><html><head><title>Recovered</title></head><body><p id="content">Loaded comments</p></body></html>');
        }
    });
    server.listen(0, '127.0.0.1');
    await once(server, 'listening');
    t.after(() => { server.closeAllConnections(); server.close(); });
    const origin = `http://127.0.0.1:${server.address().port}`;

    for (const javaScript of [true, false]) {
        for (const fragment of ['', '#comments']) {
            await t.test(`JavaScript ${javaScript ? 'enabled' : 'disabled'}, ${fragment || 'no fragment'}`, async subtest => {
                const url = `/r/example/comments/${javaScript ? 'js' : 'native'}?sort=new&context=a%2Bb&fragment=${fragment ? 'yes' : 'no'}`;
                const scenario = { requests: [] };
                cases.set(url, scenario);
                const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
                const targets = await (await fetch(`${debugOrigin}/json/list`)).json();
                const page = await CDP.connect(targets.find(target => target.id === targetId).webSocketDebuggerUrl);
                subtest.after(async () => { page.close(); await browser.send('Target.closeTarget', { targetId }); });
                await page.send('Page.enable');
                if (!javaScript) await page.send('Emulation.setScriptExecutionDisabled', { value: true });
                await page.send('Page.navigate', { url: `${origin}${url}${fragment}` });
                await waitUntil(() => page.evaluate('document.title === "Recovered"'), 'Waiting page did not send another GET after its refresh timer');
                assert.equal(await page.evaluate('location.origin + location.pathname + location.search'), `${origin}${url}`);
                assert.equal(await page.evaluate('location.hash'), '', 'Native refresh must remove the fragment to make a network navigation');
                assert.equal(scenario.requests.length, 3, 'Temporary responses must retry until the page recovers');
                assert.ok(scenario.requests.every(req => req.url === url && req.method === 'GET'));
                assert.ok(scenario.requests.slice(1).every(req => req.cookie === 'theme=dark'));
            });
        }
    }
});
