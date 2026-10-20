const { chromium } = require('playwright');
async function inspect(label, base, proxyServer) {
  const options = {executablePath:process.env.REDLIB_DIAGNOSTIC_BROWSER || '/usr/bin/chromium',headless:true};
  if (proxyServer) options.proxy = {server:proxyServer};
  const browser = await chromium.launch(options);
  const context = await browser.newContext();
  const page = await context.newPage();
  const network = [];
  page.on('response', async response => {
    try {
      const url = new URL(response.url());
      if (!/(^|\.)reddit\.com$/.test(url.hostname) && !url.hostname.endsWith('.onion')) return;
      const type = await response.headerValue('content-type') || '';
      if (type.includes('json') || /graphql|\/api\/|\/svc\//.test(url.pathname)) {
        let operation;
        try { operation = JSON.parse(response.request().postData() || '{}').operationName; } catch {}
        network.push({host:url.hostname,path:url.pathname,method:response.request().method(),status:response.status(),type,operation});
      }
    } catch {}
  });
  try {
    const response = await page.goto(base+'/r/codex/comments/1xaomu1/why_is_my_dot_mad_at_me/', {waitUntil:'domcontentloaded',timeout:40000});
    await page.waitForTimeout(4000);
    const body = await page.locator('body').innerText();
    console.log(JSON.stringify({lane:label,probe:'anonymous_browser_page',status:response?.status(),title:await page.title(),blocked:/blocked by network security/i.test(body),hasPostTitle:body.includes('Why is my Dot mad at me?'),commentNodes:await page.locator('shreddit-comment').count()}));
    console.log(JSON.stringify({lane:label,probe:'anonymous_browser_json',...await page.evaluate(async () => {
      try {
        const r = await fetch('/r/codex/comments/1xaomu1/why_is_my_dot_mad_at_me/.json?depth=1&limit=1&raw_json=1', {headers:{Accept:'application/json'},signal:AbortSignal.timeout(20000)});
        const text = await r.text(); let json; try { json=JSON.parse(text); } catch {}
        return {status:r.status,type:r.headers.get('content-type'),array:Array.isArray(json),listings:Array.isArray(json)?json.map(x=>({kind:x.kind,children:x.data?.children?.length})):null,blocked:/blocked by network security/i.test(text)};
      } catch (e) { return {error:e.name}; }
    })}));
    console.log(JSON.stringify({lane:label,probe:'anonymous_cookie_names',names:(await context.cookies()).map(c=>c.name)}));
    console.log(JSON.stringify({lane:label,probe:'anonymous_api_network',responses:network}));
  } catch (error) {
    console.log(JSON.stringify({lane:label,probe:'anonymous_browser_error',error:error.message.split('\n')[0]}));
  } finally { await context.close(); await browser.close(); }
}
(async () => {
  await inspect('direct', 'https://www.reddit.com', process.env.HTTPS_PROXY || process.env.HTTP_PROXY);
  if (process.env.REDLIB_DIAGNOSTIC_TOR === 'on') {
    await inspect('tor', 'https://www.reddittorjg6rue252oqsxryoxengawnmo46qy4kyii5wtqnwfj4ooad.onion', 'socks5://127.0.0.1:19050');
  }
})().catch(error => { console.log(JSON.stringify({error:error.message.split('\n')[0]})); process.exitCode=1; });
