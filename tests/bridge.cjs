const {test} = require('node:test');
const assert = require('node:assert/strict');
const {readFileSync} = require('node:fs');
const {webcrypto, hkdfSync, createCipheriv} = require('node:crypto');
const vm = require('node:vm');
const source = readFileSync(new URL('../src/bridge.js', `file://${__filename}`), 'utf8');
const client = '1fec8e78-bce4-4aaf-ab1b-5451cc387264';
const b64 = x => Buffer.from(x).toString('base64url');
const jwt = claims => `${b64('{}')}.${b64(JSON.stringify(claims))}.synthetic`;
const claims = {oid:'user-one',tid:'tenant-one',upn:'tester@example.test',name:'Test User',exp:4102444800,aud:'https://substrate.office.com'};
const token = {credentialType:'AccessToken',secret:jwt(claims),target:'https://substrate.office.com/SubstrateSearch-Internal.ReadWrite',clientId:client};
function environment(entries, cookie='', fetch=async()=>{throw Error('unexpected network')},origin='https://teams.microsoft.com') {
  const context = {localStorage:entries,sessionStorage:{},document:{cookie},location:{origin},crypto:webcrypto,TextEncoder,TextDecoder,Uint8Array,URL,URLSearchParams,AbortSignal,atob,fetch,setTimeout,Intl};
  return vm.runInNewContext(`(${source})`,context);
}
function encryptedFixture() {
  const master=Buffer.alloc(32,7), nonce=Buffer.alloc(16,9);
  const key=hkdfSync('sha256',master,nonce,Buffer.from(client),32);
  const cipher=createCipheriv('aes-256-gcm',key,Buffer.alloc(12));
  const data=Buffer.concat([cipher.update(JSON.stringify(token)),cipher.final(),cipher.getAuthTag()]);
  return {entry:JSON.stringify({id:'cookie-one',nonce:b64(nonce),data:b64(data)}),cookie:`msal.cache.encryption=${encodeURIComponent(JSON.stringify({id:'cookie-one',key:b64(master)}))}`};
}
const cacheKey=`user-tenant-login.microsoftonline.com-accesstoken-${client}-tenant-scope`;
test('reads modern encrypted MSAL cache without returning credentials',async()=>{
 const fixture=encryptedFixture();const run=environment({[cacheKey]:fixture.entry},fixture.cookie);
 const result=await run({action:'status',expectedAccount:'tester@example.test'});
 assert.equal(result.ok,true);assert.equal(result.profile.email,'tester@example.test');
 assert.equal(result.capabilities.search,true);assert.ok(!JSON.stringify(result).includes(token.secret));
});
test('blocks a mismatched expected account',async()=>{
 const result=await environment({[cacheKey]:JSON.stringify(token)})({action:'profile',expectedAccount:'other@example.test'});
 assert.equal(result.error.code,'ACCOUNT_MISMATCH');
});
test('rejects expired credentials instead of launching a login',async()=>{
 const old={...token,secret:jwt({...claims,exp:1})};
 const result=await environment({[cacheKey]:JSON.stringify(old)})({action:'profile'});
 assert.equal(result.error.code,'AUTH_REQUIRED');
});
test('checks exact origin before reading browser storage',async()=>{
 const run=environment({},'',undefined,'https://teams.microsoft.com.attacker.test');
 const result=await run({action:'profile'});assert.equal(result.error.code,'WRONG_ORIGIN');
});
test('does not send credentials to a discovered foreign host',async()=>{
 const storage={[cacheKey]:JSON.stringify(token), 'DISCOVER-REGION-GTM':JSON.stringify({item:{chatServiceAfd:'https://attacker.test/api/chatsvc/amer'}})};
 const result=await environment(storage)({action:'request',request:{service:'chat',path:'/v1/users/ME/conversations',method:'GET',query:[],body:null}});
 assert.equal(result.error.code,'UNSAFE_ENDPOINT');
});
test('search fetch stays in browser and does not leak error response body',async()=>{
 let calls=0;const run=environment({[cacheKey]:JSON.stringify(token)},'',async(url,options)=>{
  calls++;assert.equal(new URL(url).hostname,'substrate.office.com');assert.equal(options.headers.Authorization,`Bearer ${token.secret}`);
  assert.equal(options.redirect,'error');assert.equal(options.credentials,'omit');
  return {ok:false,status:401,headers:new Headers(),text:async()=>token.secret};
 });
 const result=await run({action:'request',request:{service:'substrate',path:'/searchservice/api/v2/query',method:'POST',query:[],body:{entityRequests:[]}}});
 assert.equal(calls,1);assert.equal(result.error.code,'AUTH_EXPIRED');assert.ok(!JSON.stringify(result).includes(token.secret));
});

test('reuses Teams encrypted resource cache in browser and desktop webviews',async()=>{
 const key=Buffer.alloc(32,4),iv=Buffer.alloc(16,2);
 const resourceToken=jwt({...claims,aud:'https://api.spaces.skype.com'});
 const cipher=createCipheriv('aes-256-cbc',key,iv);
 const encrypted=Buffer.concat([cipher.update(resourceToken),cipher.final()]);
 const storage={
  'tmp.auth.v1.GLOBAL.ExportedEncryptionKey.ExportedEncryptionKey':JSON.stringify({item:{exportedKey:key.toString('base64')}}),
  'tmp.auth.v1.user-one.Token.HTTPS://API.SPACES.SKYPE.COM':JSON.stringify({item:{token:'',encryptedToken:encrypted.toString('base64'),iv:iv.toString('base64')}})
 };
 const result=await environment(storage)({action:'status',expectedAccount:'tester@example.test'});
 assert.equal(result.ok,true);assert.equal(result.capabilities.chat,true);
 assert.ok(!JSON.stringify(result).includes(resourceToken));
});

test('selects the active tenant and never mixes chat credentials from another tenant',async()=>{
 const foreign={...token,secret:jwt({...claims,oid:'user-two',tid:'tenant-two',aud:'https://api.spaces.skype.com'}),target:'https://api.spaces.skype.com/.default'};
 const storage={[cacheKey]:JSON.stringify(token),['other-accesstoken']:JSON.stringify(foreign),'msal.client.active-account-filters':JSON.stringify({localAccountId:'user-one',tenantId:'tenant-one'})};
 const result=await environment(storage)({action:'status',expectedAccount:'tester@example.test'});
 assert.equal(result.ok,true);assert.equal(result.profile.tenantId,'tenant-one');assert.equal(result.capabilities.chat,false);
});
test('ambiguous accounts require explicit selection',async()=>{
 const other={...token,secret:jwt({...claims,oid:'user-two',tid:'tenant-two'})};
 const result=await environment({[cacheKey]:JSON.stringify(token),'other-accesstoken':JSON.stringify(other)})({action:'status'});
 assert.equal(result.error.code,'ACCOUNT_AMBIGUOUS');
});
test('unknown paths and writes never reach fetch',async()=>{
 for(const request of [
  {service:'substrate',path:'/searchservice/api/v2/query',method:'DELETE'},
  {service:'substrate',path:'//evil.test',method:'POST'},
  {service:'chat',path:'/v1/users/ME/conversations/../messages',method:'GET'}
 ]) {
  const result=await environment({[cacheKey]:JSON.stringify(token)})({action:'request',request});
  assert.ok(['UNSAFE_ENDPOINT','REGION_UNAVAILABLE'].includes(result.error.code));
 }
});
test('successful search is bounded JSON and preserves no auth material',async()=>{
 const fetch=async()=>new Response(JSON.stringify({EntitySets:[]}),{headers:{'Content-Type':'application/json'}});
 const result=await environment({[cacheKey]:JSON.stringify(token)},'',fetch)({action:'request',request:{service:'substrate',path:'/searchservice/api/v2/query',method:'POST',query:[],body:{}}});
 assert.equal(result.ok,true);assert.equal(result.data.EntitySets.length,0);assert.ok(!JSON.stringify(result).includes(token.secret));
});

test('guest people search limitation has an actionable sanitized error',async()=>{
 const body={error:{message:'No providers were enabled for the request (partner: teams Scenario:powerbar GuestAccess: True) private-marker'}};
 const fetch=async()=>new Response(JSON.stringify(body),{status:400});
 const result=await environment({[cacheKey]:JSON.stringify(token)},'',fetch)({action:'request',request:{service:'substrate',path:'/search/api/v1/suggestions',method:'POST',query:[],body:{}}});
 assert.equal(result.error.code,'FEATURE_UNAVAILABLE');
 assert.ok(!JSON.stringify(result).includes('private-marker'));
});


test('native app uses its own authenticated bridge key without exposing it', async()=>{
 const key=Buffer.alloc(32,6),iv=Buffer.alloc(16,3);
 const secret=jwt({...claims,aud:'https://api.spaces.skype.com'});
 const cipher=createCipheriv('aes-256-cbc',key,iv);
 const storage={
  'tmp.auth.v1.GLOBAL.User.User':JSON.stringify({item:{profile:claims}}),
  'tmp.auth.v1.user-one.Token.HTTPS://API.SPACES.SKYPE.COM':JSON.stringify({item:{token:'',encryptedToken:Buffer.concat([cipher.update(secret),cipher.final()]).toString('base64'),iv:iv.toString('base64')}})
 };
 let nativeCalls=0;
 const context={localStorage:storage,sessionStorage:{},document:{cookie:''},location:{origin:'https://teams.microsoft.com'},crypto:webcrypto,TextEncoder,TextDecoder,Uint8Array,URL,URLSearchParams,AbortSignal,atob,fetch:async()=>{throw Error('unexpected network')},nativeModules:{Auth:{acquireEncryptionKey:async args=>{nativeCalls++;assert.ok(args.correlationId);return {result:key.toString('base64')}}}}};
 const run=vm.runInNewContext(`(${source})`,context);
 const result=await run({action:'status',nativeApp:true});
 assert.equal(result.ok,true);assert.equal(result.capabilities.chat,true);assert.equal(nativeCalls,1);
 assert.ok(!JSON.stringify(result).includes(secret));assert.ok(!JSON.stringify(result).includes(key.toString('base64')));
 assert.equal(storage['tmp.auth.v1.GLOBAL.ExportedEncryptionKey.ExportedEncryptionKey'],undefined);
});

test('native expired active identity cannot switch to another cached tenant',async()=>{
 const storage={
  [cacheKey]:JSON.stringify({...token,secret:jwt({...claims,exp:1})}),
  'other-accesstoken':JSON.stringify({...token,secret:jwt({...claims,oid:'other-user',tid:'other-tenant'})}),
  'tmp.auth.v1.GLOBAL.User.User':JSON.stringify({item:{profile:claims}}),
  'msal.other.active-account-filters':JSON.stringify({localAccountId:'other-user',tenantId:'other-tenant'})
 };
 const context={localStorage:storage,sessionStorage:{},document:{cookie:''},location:{origin:'https://teams.microsoft.com'},crypto:webcrypto,TextEncoder,TextDecoder,Uint8Array,URL,AbortSignal,atob,fetch:async()=>{throw Error('unexpected network')},nativeModules:{Auth:{acquireEncryptionKey:async()=>({result:Buffer.alloc(32,6).toString('base64')})}}};
 const result=await vm.runInNewContext(`(${source})`,context)({action:'status',nativeApp:true});
 assert.equal(result.error.code,'AUTH_REQUIRED');
});

test('native bridge is not called when the app window has navigated to another origin',async()=>{
 let called=false;
 const context={localStorage:{},sessionStorage:{},document:{cookie:''},location:{origin:'https://attacker.test'},crypto:webcrypto,TextEncoder,TextDecoder,Uint8Array,URL,AbortSignal,atob,nativeModules:{Auth:{acquireEncryptionKey:async()=>{called=true;return {result:'private-key'}}}}};
 const result=await vm.runInNewContext(`(${source})`,context)({action:'status',nativeApp:true});
 assert.equal(result.error.code,'WRONG_ORIGIN');assert.equal(called,false);
});

test('native host errors cannot impersonate safe connector errors',async()=>{
 const context={localStorage:{},sessionStorage:{},document:{cookie:''},location:{origin:'https://teams.microsoft.com'},crypto:webcrypto,TextEncoder,TextDecoder,Uint8Array,URL,AbortSignal,atob,nativeModules:{Auth:{acquireEncryptionKey:async()=>{throw {code:'AUTH_REQUIRED',message:'synthetic-native-secret-marker'}}}}};
 const result=await vm.runInNewContext(`(${source})`,context)({action:'status',nativeApp:true});
 assert.equal(result.ok,false);
 assert.ok(!JSON.stringify(result).includes('synthetic-native-secret-marker'));
 assert.equal(result.error.code,'SESSION_UNAVAILABLE');
});

test('a tenant constraint cannot switch away from the expired native active account',async()=>{
 const storage={
  [cacheKey]:JSON.stringify({...token,secret:jwt({...claims,exp:1})}),
  'other-accesstoken':JSON.stringify({...token,secret:jwt({...claims,oid:'other-user',upn:'other@example.test'})}),
  'tmp.auth.v1.GLOBAL.User.User':JSON.stringify({item:{profile:claims}})
 };
 const context={localStorage:storage,sessionStorage:{},document:{cookie:''},location:{origin:'https://teams.microsoft.com'},crypto:webcrypto,TextEncoder,TextDecoder,Uint8Array,URL,AbortSignal,atob,nativeModules:{Auth:{acquireEncryptionKey:async()=>({result:Buffer.alloc(32,6).toString('base64')})}}};
 const run=vm.runInNewContext(`(${source})`,context);
 const result=await run({action:'status',nativeApp:true,tenantId:claims.tid});
 assert.equal(result.ok,false);
 assert.equal(result.error.code,'AUTH_REQUIRED');
 const explicit=await run({action:'status',nativeApp:true,tenantId:claims.tid,expectedAccount:'other@example.test'});
 assert.equal(explicit.ok,true);
 assert.equal(explicit.profile.id,'other-user');
});

test('credential export is not a supported bridge operation',async()=>{
 const storage={
  [cacheKey]:JSON.stringify({...token,homeAccountId:'home-one',secret:jwt({...claims,appid:client})}),
  'own-refreshtoken':JSON.stringify({credentialType:'RefreshToken',clientId:client,homeAccountId:'home-one',secret:'own-refresh-marker'}),
  'foreign-refreshtoken':JSON.stringify({credentialType:'RefreshToken',clientId:client,homeAccountId:'home-other',secret:'foreign-refresh-marker'}),
  'other-client-refreshtoken':JSON.stringify({credentialType:'RefreshToken',clientId:'other-client',homeAccountId:'home-one',secret:'other-client-refresh-marker'})
 };
 const run=environment(storage);
 const result=await run({action:'export-session'});
 assert.equal(result.ok,false);
 assert.equal(result.error.code,'INVALID_INPUT');
 assert.ok(!JSON.stringify(result).includes('own-refresh-marker'));
 assert.ok(!JSON.stringify(result).includes('foreign-refresh-marker'));
 assert.ok(!JSON.stringify(result).includes('other-client-refresh-marker'));
 const status=await run({action:'status'});
 assert.ok(!JSON.stringify(status).includes('own-refresh-marker'));
});


const spaces={...token,secret:jwt({...claims,aud:'https://api.spaces.skype.com'}),target:'https://api.spaces.skype.com/.default'};
const region=(base='https://teams.microsoft.com/api/chatsvc/amer')=>({'DISCOVER-REGION-GTM':JSON.stringify({item:{chatServiceAfd:base,chatSvcAggAfd:'https://teams.microsoft.com/api/csa/amer'}})});
const json=(body,init={})=>new Response(JSON.stringify(body),{headers:{'Content-Type':'application/json'},...init});
const chatRequest={service:'chat',path:'/v1/users/ME/conversations',method:'GET',query:[],body:null};
const chatStorage=extra=>({[cacheKey]:JSON.stringify(token),'spaces-accesstoken':JSON.stringify(spaces),...extra});

test('reads only the storage values it needs instead of copying all storage',async()=>{
 const reads=[];const entries=new Proxy({[cacheKey]:JSON.stringify(token),'unrelated-large-value':'x'.repeat(4096)},{get(target,key){if(typeof key==='string')reads.push(key);return target[key];}});
 const result=await environment(entries)({action:'status'});
 assert.equal(result.ok,true);assert.ok(reads.includes(cacheKey));assert.ok(!reads.includes('unrelated-large-value'));
});
test('decrypts unchanged encrypted entries once across calls',async()=>{
 const fixture=encryptedFixture();let decrypts=0;
 const subtle=new Proxy(webcrypto.subtle,{get(target,key){const value=target[key];if(key==='decrypt')return (...args)=>{decrypts++;return value.apply(target,args);};return typeof value==='function'?value.bind(target):value;}});
 const context={localStorage:{[cacheKey]:fixture.entry},sessionStorage:{},document:{cookie:fixture.cookie},location:{origin:'https://teams.microsoft.com'},crypto:{subtle,randomUUID:()=>webcrypto.randomUUID()},TextEncoder,TextDecoder,Uint8Array,URL,AbortSignal,atob,setTimeout};
 const run=vm.runInNewContext(`(${source})`,context);
 assert.equal((await run({action:'status'})).ok,true);const first=decrypts;
 assert.equal((await run({action:'status'})).ok,true);
 assert.ok(first>0);assert.equal(decrypts,first);
});
test('reuses one Skype token per account until expiry and renews it once after 401',async()=>{
 const calls={authz:0,data:0};let reject=false;
 const run=environment(chatStorage(region()),'',async url=>{
  if(new URL(url).hostname==='authsvc.teams.microsoft.com'){calls.authz++;return json({tokens:{skypeToken:`skype-${calls.authz}`,expiresIn:3600}});}
  calls.data++;if(reject){reject=false;return json({},{status:401});}return json({conversations:[]});
 });
 for(let i=0;i<3;i++) assert.equal((await run({action:'request',request:chatRequest})).ok,true);
 assert.deepEqual(calls,{authz:1,data:3});
 reject=true;const renewed=await run({action:'request',request:chatRequest});
 assert.equal(renewed.ok,true);assert.deepEqual(calls,{authz:2,data:5});
 assert.ok(!JSON.stringify(renewed).includes('skype-'));
});
test('retries one transient failure or short rate limit, then reports the error',async()=>{
 const statuses=[429,200];const seen=[];
 const run=environment({[cacheKey]:JSON.stringify(token)},'',async()=>{const status=statuses.shift()??500;seen.push(status);return status===200?json({EntitySets:[]}):json({},{status,headers:{'Retry-After':'0'}});});
 const search={action:'request',request:{service:'substrate',path:'/searchservice/api/v2/query',method:'POST',query:[],body:{}}};
 assert.equal((await run(search)).ok,true);assert.deepEqual(seen,[429,200]);
 seen.length=0;const failed=await run(search);
 assert.equal(failed.error.code,'API_ERROR');assert.deepEqual(seen,[500,500]);
 const slow=environment({[cacheKey]:JSON.stringify(token)},'',async()=>json({},{status:429,headers:{'Retry-After':'120'}}));
 assert.equal((await slow(search)).error.code,'RATE_LIMITED');
});
test('falls back to the token exchange region only for trusted Teams roots',async()=>{
 const urls=[];const run=base=>environment(chatStorage(),'',async url=>{urls.push(url);return new URL(url).hostname==='authsvc.teams.microsoft.com'?json({tokens:{skypeToken:'s',expiresIn:600},regionGtms:{chatServiceAfd:base}}):json({conversations:[]});});
 assert.equal((await run('https://teams.microsoft.com/api/chatsvc/emea')({action:'request',request:chatRequest})).ok,true);
 assert.ok(urls.some(url=>url.startsWith('https://teams.microsoft.com/api/chatsvc/emea/v1/users/ME/conversations')));
 urls.length=0;const foreign=await run('https://attacker.test/api/chatsvc/emea')({action:'request',request:chatRequest});
 assert.equal(foreign.error.code,'UNSAFE_ENDPOINT');assert.ok(urls.every(url=>!url.includes('attacker.test')));
});
test('batches validated reads and reports per-request errors',async()=>{
 const run=environment(chatStorage(region()),'',async url=>new URL(url).hostname==='authsvc.teams.microsoft.com'?json({tokens:{skypeToken:'s',expiresIn:600}}):json({messages:[]}));
 const result=await run({action:'batch',requests:[{...chatRequest,path:'/v1/users/ME/conversations/48%3Anotifications/messages'},{...chatRequest,path:'/v1/users/ME/conversations/../x'}]});
 assert.equal(result.ok,true);assert.equal(result.data[0].ok,true);assert.equal(result.data[1].error.code,'UNSAFE_ENDPOINT');
 assert.equal((await run({action:'batch',requests:[]})).error.code,'INVALID_INPUT');
});
test('conversation folders are an allowed read and other CSA paths are not',async()=>{
 const csa={...token,secret:jwt({...claims,aud:'https://chatsvcagg.teams.microsoft.com'}),target:'https://chatsvcagg.teams.microsoft.com/.default'};
 const run=environment(chatStorage({...region(),'csa-accesstoken':JSON.stringify(csa)}),'',async url=>new URL(url).hostname==='authsvc.teams.microsoft.com'?json({tokens:{skypeToken:'s',expiresIn:600}}):json({conversationFolders:[]}));
 assert.equal((await run({action:'request',request:{service:'csa',path:'/api/v1/teams/users/me/conversationFolders',method:'GET',query:[],body:null}})).ok,true);
 assert.equal((await run({action:'request',request:{service:'csa',path:'/api/v1/teams/users/me/other',method:'GET',query:[],body:null}})).error.code,'UNSAFE_ENDPOINT');
});
