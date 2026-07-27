import { chromium } from 'playwright';

// JSON-RPC 协议
// 输入 (stdin): {"id":1,"method":"open","params":{"url":"..."}}
// 输出 (stdout): {"id":1,"result":{...}} 或 {"id":1,"error":"..."}

let browser = null;
let page = null;

async function handleRequest(req) {
    const { id, method, params } = req;
    try {
        let result;
        switch (method) {
            case 'open': {
                const { url, width = 1280, height = 720 } = params || {};
                if (!browser) {
                    browser = await chromium.launch({ headless: true });
                }
                const context = await browser.newContext({ viewport: { width, height } });
                page = await context.newPage();
                if (url) {
                    await page.goto(url, { waitUntil: 'networkidle', timeout: 30000 });
                }
                result = { url: page.url(), title: await page.title() };
                break;
            }
            case 'goto': {
                const { url, timeout = 30000 } = params || {};
                if (!page) throw new Error('No page open');
                await page.goto(url, { waitUntil: 'networkidle', timeout });
                result = { url: page.url(), title: await page.title() };
                break;
            }
            case 'click': {
                const { selector, timeout = 5000 } = params || {};
                if (!page) throw new Error('No page open');
                await page.click(selector, { timeout });
                result = { clicked: selector };
                break;
            }
            case 'type': {
                const { selector, text, timeout = 5000 } = params || {};
                if (!page) throw new Error('No page open');
                await page.fill(selector, '', { timeout });
                await page.type(selector, text, { delay: 10, timeout });
                result = { typed: selector, text };
                break;
            }
            case 'fill': {
                const { selector, text, timeout = 5000 } = params || {};
                if (!page) throw new Error('No page open');
                await page.fill(selector, text, { timeout });
                result = { filled: selector };
                break;
            }
            case 'extract': {
                if (!page) throw new Error('No page open');
                const title = await page.title();
                const text = await page.evaluate(() => document.body?.innerText || '');
                const html = await page.content();
                result = { title, text: text.slice(0, 50000), url: page.url() };
                break;
            }
            case 'evaluate': {
                const { script } = params || {};
                if (!page) throw new Error('No page open');
                const value = await page.evaluate(script);
                result = { value };
                break;
            }
            case 'screenshot': {
                if (!page) throw new Error('No page open');
                const buf = await page.screenshot({ type: 'png', fullPage: false });
                result = { data: buf.toString('base64'), mimeType: 'image/png', bytes: buf.length };
                break;
            }
            case 'screenshot_full': {
                if (!page) throw new Error('No page open');
                const buf = await page.screenshot({ type: 'png', fullPage: true });
                result = { data: buf.toString('base64'), mimeType: 'image/png', bytes: buf.length };
                break;
            }
            case 'close': {
                if (browser) {
                    await browser.close();
                    browser = null;
                    page = null;
                }
                result = { closed: true };
                break;
            }
            default:
                throw new Error(`Unknown method: ${method}`);
        }
        process.stdout.write(JSON.stringify({ id, result }) + '\n');
    } catch (e) {
        process.stdout.write(JSON.stringify({ id, error: e.message }) + '\n');
    }
}

// 从 stdin 读取 JSON-RPC 请求
let buffer = '';
process.stdin.on('data', (chunk) => {
    buffer += chunk.toString();
    const lines = buffer.split('\n');
    buffer = lines.pop() || '';
    for (const line of lines) {
        if (line.trim()) {
            try {
                const req = JSON.parse(line);
                handleRequest(req);
            } catch (e) {
                process.stdout.write(JSON.stringify({ error: `parse error: ${e.message}` }) + '\n');
            }
        }
    }
});

process.stdin.on('close', async () => {
    if (browser) {
        try { await browser.close(); } catch {}
    }
    process.exit(0);
});

// 初始化完成
process.stdout.write(JSON.stringify({ ready: true }) + '\n');
