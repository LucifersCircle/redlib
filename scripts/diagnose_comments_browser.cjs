const { chromium } = require('playwright');
const { spawn } = require('node:child_process');
async function anonymousCookieTokenProbe(context, base, proxyServer) {
  const cookie=(await context.cookies()).find(c=>c.name==='token_v2');
  if(!cookie) return {available:false};
  const token=decodeURIComponent(cookie.value);
  const args=['--silent','--show-error','--max-time','35','--max-filesize','8388608','--write-out','\n%{http_code}','--config','-'];
  if(proxyServer) args.push('--proxy',proxyServer.replace('socks5://','socks5h://'));
  args.push(base.replace('https://www.','https://oauth.')+'/r/codex/comments/1xaomu1.json?depth=1&limit=1&raw_json=1');
  return new Promise(resolve=>{
    const child=spawn('curl',args);let output='';
    child.stdout.on('data',d=>{if(output.length<8388608)output+=d.toString()});
    child.stderr.resume();
    child.on('error',()=>resolve({available:true,transportError:true}));
    child.on('close',code=>{const cut=output.lastIndexOf('\n');const text=output.slice(0,cut);let json;try{json=JSON.parse(text)}catch{}resolve({available:true,exit:code,status:Number(output.slice(cut+1)),array:Array.isArray(json),listings:Array.isArray(json)?json.map(x=>({kind:x.kind,children:x.data?.children?.length})):null,blocked:/blocked by network security/i.test(text)})});
    child.stdin.end('header = '+JSON.stringify('Authorization: Bearer '+token)+'\nheader = "Accept: application/json"\nheader = "User-Agent: linux:LucifersCircle.redlib:0.36.0 (https://github.com/LucifersCircle/redlib)"\n');
  });
}
async function inspect(label, base, proxyServer) {
  const options = {executablePath:process.env.REDLIB_DIAGNOSTIC_BROWSER || '/usr/bin/chromium',headless:process.env.REDLIB_DIAGNOSTIC_HEADED !== 'on'};
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
        network.push({host:url.hostname,path:url.pathname,queryKeys:[...url.searchParams.keys()],sort:url.searchParams.get('sort'),method:response.request().method(),status:response.status(),type,operation});
      }
    } catch {}
  });
  try {
    const home = await page.goto(base+'/', {waitUntil:'domcontentloaded',timeout:40000});
    await page.waitForTimeout(4000);
    console.log(JSON.stringify({lane:label,probe:'anonymous_home_bootstrap',status:home?.status(),cookieNames:(await context.cookies()).map(c=>c.name)}));
    if (home?.status() === 429) return;
    const response = await page.goto(base+'/r/codex/comments/1xaomu1/why_is_my_dot_mad_at_me/', {waitUntil:'domcontentloaded',timeout:40000});
    await page.waitForTimeout(4000);
    const body = await page.locator('body').innerText();
    console.log(JSON.stringify({lane:label,probe:'anonymous_browser_page',status:response?.status(),title:await page.title(),blocked:/blocked by network security/i.test(body),hasPostTitle:body.includes('Why is my Dot mad at me?'),commentNodes:await page.locator('shreddit-comment').count()}));
    if (response?.status() === 200 && await page.locator('shreddit-comment').count()) {
      console.log(JSON.stringify({lane:label,probe:'anonymous_comment_structure',...await page.evaluate(() => {
        const shape = selector => [...document.querySelectorAll(selector)].slice(0,3).map(e => ({tag:e.tagName.toLowerCase(),attributes:e.getAttributeNames(),children:[...e.children].slice(0,8).map(c=>({tag:c.tagName.toLowerCase(),slot:c.getAttribute('slot'),id:c.id || undefined})),publicFields:Object.fromEntries(['thingid','post-id','parent-id','depth','score','comment-count','created-timestamp'].filter(k=>e.hasAttribute(k)).map(k=>[k,e.getAttribute(k)]))}));
        return {posts:shape('shreddit-post'),comments:shape('shreddit-comment'),trees:shape('shreddit-comment-tree'),jsonScripts:[...document.querySelectorAll('script[type="application/json"]')].map(e=>{let j;try{j=JSON.parse(e.textContent)}catch{}return {id:e.id,keys:j&&typeof j==='object'?Object.keys(j).slice(0,20):[],containsPost:e.textContent.includes('1xaomu1')}}),sortControls:[...document.querySelectorAll('[id*="sort"],[name*="sort"],comment-sort-dropdown')].slice(0,15).map(e=>({tag:e.tagName.toLowerCase(),id:e.id,name:e.getAttribute('name'),text:e.textContent.trim().slice(0,120)}))};
      })}));
      const get = async suffix => page.evaluate(async suffix => {
        try {const r=await fetch(location.pathname+suffix,{signal:AbortSignal.timeout(30000)});const t=await r.text();const doc=new DOMParser().parseFromString(t,'text/html');return {status:r.status,type:r.headers.get('content-type'),comments:doc.querySelectorAll('shreddit-comment').length,posts:doc.querySelectorAll('shreddit-post').length,blocked:/blocked by network security/i.test(t)}}catch(e){return {error:e.name}}
      },suffix);
      console.log(JSON.stringify({lane:label,probe:'anonymous_html_fetch',...await get('')}));
      console.log(JSON.stringify({lane:label,probe:'anonymous_html_sort_new',...await get('?sort=new')}));
      console.log(JSON.stringify({lane:label,probe:'anonymous_web_token_comments',...await anonymousCookieTokenProbe(context,base,proxyServer)}));
      const sortButton=page.locator('#comment-sort-button');
      if(await sortButton.count()) {
        await sortButton.first().click({timeout:5000});
        const newSort=page.getByRole('menuitem',{name:'New',exact:true});
        if(await newSort.count()) {await newSort.first().click({timeout:5000});await page.waitForTimeout(5000);}
      }
    }
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
