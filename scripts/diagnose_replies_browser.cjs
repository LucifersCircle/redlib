const {chromium}=require('playwright');
(async()=>{
 const base='https://www.reddittorjg6rue252oqsxryoxengawnmo46qy4kyii5wtqnwfj4ooad.onion';
 const browser=await chromium.launch({executablePath:'/usr/bin/google-chrome',headless:false,proxy:{server:'socks5://127.0.0.1:19050'}});
 try {
  const page=await browser.newPage();
  const home=await page.goto(base+'/',{waitUntil:'commit',timeout:60000});
  console.log(JSON.stringify({probe:'browser_home_commit',status:home.status()}));
  await page.waitForTimeout(1500);
  const thread=await page.goto(base+'/r/codex/comments/1xaomu1/why_is_my_dot_mad_at_me/',{waitUntil:'commit',timeout:60000});
  console.log(JSON.stringify({probe:'browser_thread_commit',status:thread.status()}));
  const loader=page.locator('faceplate-partial[src*="/svc/shreddit/more-comments/"]').first();
  await loader.waitFor({state:'attached',timeout:90000});
  const source=await loader.getAttribute('src');const url=new URL(source,base);
  const params=Object.fromEntries(['sort','startingDepth','commentParentPositions'].map(k=>[k,url.searchParams.get(k)]));
  console.log(JSON.stringify({probe:'browser_reply_source',path:url.pathname,params,before:await page.locator('shreddit-comment').count()}));
  const pending=page.waitForResponse(r=>new URL(r.url()).pathname.includes('/svc/shreddit/more-comments/'),{timeout:25000});
  const button=loader.getByRole('button');
  if(await button.count()) await button.first().click({timeout:8000});
  else await loader.getByText(/more replies/i).first().click({timeout:8000});
  const response=await pending;const body=await response.text();const headers=await response.request().allHeaders();
  const allowed=['accept','x-requested-with','sec-fetch-dest','sec-fetch-mode','sec-fetch-site'];
  console.log(JSON.stringify({probe:'browser_reply_response',status:response.status(),type:await response.headerValue('content-type'),rawComments:(body.match(/<shreddit-comment(?:\s|>)/g)||[]).length,containsCommentTag:body.includes('<shreddit-comment'),firstTag:(body.match(/<([a-zA-Z][\w-]*)/)||[])[1],jsonKeys:(()=>{try{return Object.keys(JSON.parse(body))}catch{return []}})(),headerNames:Object.keys(headers),fixedHeaders:Object.fromEntries(allowed.filter(k=>k in headers).map(k=>[k,headers[k]]))}));
  await page.waitForTimeout(3000);
  console.log(JSON.stringify({probe:'browser_reply_result',after:await page.locator('shreddit-comment').count()}));
 }catch(e){console.log(JSON.stringify({probe:'browser_reply_error',error:e.message.split('\n')[0]}));process.exitCode=1}
 finally {await browser.close()}
})().catch(e=>{console.log(JSON.stringify({error:e.name}));process.exitCode=1});
