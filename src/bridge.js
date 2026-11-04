(() => {
    // Runs only inside the selected Teams page. No credential leaves this function.
    // These caches live in this closure, reachable only through the connector's own
    // DevTools handle; page scripts cannot read them. Identity checks stay per call.
    const origins = ['https://teams.microsoft.com', 'https://teams.cloud.microsoft'];
    const claimsCache = new Map(), decryptCache = new Map(), keyCache = new Map(), skypeCache = new Map();
    const remember = (cache, key, value) => { if (cache.size >= 512) cache.clear(); cache.set(key, value); return value; };

    return async function teamsBridge(input) {
        const failures = new WeakSet();
        const fail = (code, message) => { const error = {code, message}; failures.add(error); throw error; };
        const decode = value => Uint8Array.from(atob(value.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
        const parse = value => { try { return JSON.parse(value); } catch { return null; } };
        const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
        const payload = value => {
            if (typeof value !== 'string') return {};
            if (claimsCache.has(value)) return claimsCache.get(value);
            let claims;
            try { claims = parse(new TextDecoder().decode(decode(value.split('.')[1]))) || {}; } catch { claims = {}; }
            return remember(claimsCache, value, claims);
        };
        const email = claims => claims.preferred_username || claims.upn || claims.email || '';
        const valid = entry => Number(entry.expiresOn || payload(entry.secret).exp) > Date.now() / 1000 + 30;

        function storageView() {
            // Enumerate keys only; values are read on demand for the few keys the bridge uses.
            const item = (storage, key) => typeof storage.getItem === 'function' ? storage.getItem(key) : storage[key];
            const keys = [...new Set([...Object.keys(localStorage), ...Object.keys(sessionStorage)])];
            const read = key => { const value = item(sessionStorage, key); return typeof value === 'string' ? value : item(localStorage, key); };
            return {keys, read};
        }

        async function importKey(raw, algorithm, usage) {
            const id = `${algorithm}:${raw}`;
            if (keyCache.has(id)) return keyCache.get(id);
            return remember(keyCache, id, await crypto.subtle.importKey('raw', decode(raw), algorithm, false, [usage]));
        }

        async function readTokens(view) {
            const cookieText = document.cookie.split(';').map(s => s.trim()).find(s => s.startsWith('msal.cache.encryption='));
            const cookie = cookieText && parse(decodeURIComponent(cookieText.slice(cookieText.indexOf('=') + 1)));
            const base = cookie?.key && await importKey(cookie.key, 'HKDF', 'deriveKey');
            const clients = view.keys.filter(k => /^msal\.(?:\d+\.)?token\.keys\./.test(k)).map(k => k.split('.').at(-1));
            const entries = [];
            for (const key of view.keys.filter(k => /(?:access|id)token/i.test(k))) {
                const raw = view.read(key);
                let entry = parse(raw);
                if (entry?.id && entry.nonce && entry.data) {
                    if (!base || entry.id !== cookie.id) continue;
                    const id = `msal\u0000${cookie.id}\u0000${key}\u0000${raw}`;
                    if (decryptCache.has(id)) entry = decryptCache.get(id);
                    else {
                        const legacyClient = key.match(/-(?:access|id|refresh)token-([a-f\d-]{36})-/i)?.[1];
                        entry = remember(decryptCache, id, await decrypt(entry, base, [...clients.filter(c => key.includes(c)), legacyClient, ''].filter(c => c !== undefined)));
                    }
                }
                if (entry?.secret && (['AccessToken', 'IdToken'].includes(entry.credentialType) && valid(entry))) entries.push(entry);
            }
            return entries;
        }

        async function readTeamsTokens(view) {
            let exported;
            if (input.nativeApp) {
                const auth = globalThis.nativeModules?.Auth;
                if (typeof auth?.acquireEncryptionKey !== 'function') fail('APP_BRIDGE_UNAVAILABLE', 'The selected Teams app window has not loaded its authentication bridge. Select the main Teams window and retry.');
                exported = (await auth.acquireEncryptionKey({correlationId:crypto.randomUUID()}))?.result;
                if (typeof exported !== 'string' || !exported) fail('APP_BRIDGE_UNAVAILABLE', 'The Teams app could not provide its session encryption key. Finish signing in inside the app and retry.');
            } else {
                exported = parse(view.read('tmp.auth.v1.GLOBAL.ExportedEncryptionKey.ExportedEncryptionKey'))?.item?.exportedKey;
            }
            const key = exported && await importKey(exported, 'AES-CBC', 'decrypt');
            const entries = [];
            for (const name of view.keys.filter(k => /\.Token\.HTTPS:\/\/(?:API\.SPACES\.SKYPE\.COM|CHATSVCAGG\.TEAMS\.MICROSOFT\.COM|SUBSTRATE\.OFFICE\.COM|OUTLOOK\.OFFICE\.COM)/.test(k))) {
                try {
                    const raw = view.read(name);
                    const item = parse(raw)?.item;
                    let secret = item?.token;
                    if (!secret && key && item?.encryptedToken && item.iv) {
                        const id = `teams\u0000${exported}\u0000${name}\u0000${raw}`;
                        secret = decryptCache.has(id) ? decryptCache.get(id)
                            : remember(decryptCache, id, new TextDecoder().decode(await crypto.subtle.decrypt({name:'AES-CBC', iv:decode(item.iv)}, key, decode(item.encryptedToken))));
                    }
                    if (!secret) continue;
                    const claims = payload(secret);
                    const entry = {credentialType:'AccessToken', tokenType:'Bearer', secret, expiresOn:claims.exp,
                        target:`${name.split('.Token.')[1].toLowerCase()}/${claims.scp || ''}`};
                    if (valid(entry)) entries.push(entry);
                } catch { /* Stale encrypted entries stay owned by Teams; never alter its cache. */ }
            }
            return entries;
        }

        async function decrypt(entry, base, contexts) {
            // Microsoft's MSAL cache format: HKDF SHA-256, nonce salt, client-id context, AES-GCM with zero IV.
            for (const context of new Set(contexts)) {
                try {
                    const key = await crypto.subtle.deriveKey({name:'HKDF', salt:decode(entry.nonce), hash:'SHA-256', info:new TextEncoder().encode(context)}, base, {name:'AES-GCM', length:256}, false, ['decrypt']);
                    return parse(new TextDecoder().decode(await crypto.subtle.decrypt({name:'AES-GCM', iv:new Uint8Array(12)}, key, decode(entry.data))));
                } catch { /* A cache entry may belong to an earlier session or client. */ }
            }
            return null;
        }

        function selectAccount(tokens, view) {
            const candidates = tokens.map(entry => ({entry, claims:payload(entry.secret)})).filter(x => x.claims.oid && x.claims.tid);
            const appUser = input.nativeApp && parse(view.read('tmp.auth.v1.GLOBAL.User.User'))?.item;
            const appProfile = appUser?.profile;
            const hasAppIdentity = appProfile?.oid && appProfile?.tid;
            if (input.nativeApp && !hasAppIdentity && !(input.expectedAccount && input.tenantId)) fail('ACCOUNT_SELECTION_REQUIRED', 'The Teams app active account cannot be established. Set TEAMS_ACCOUNT and TEAMS_TENANT_ID explicitly.');
            const active = hasAppIdentity ? [{localAccountId:appProfile.oid,tenantId:appProfile.tid}]
                : view.keys.filter(k => k.endsWith('.active-account-filters')).map(k => parse(view.read(k))).filter(Boolean);
            const appAlias = x => hasAppIdentity && x.oid === appProfile.oid && x.tid === appProfile.tid ? (appUser.userName || email(appProfile)) : '';
            let selected = candidates.filter(x => !input.expectedAccount || [email(x.claims),appAlias(x.claims)].some(value => value.toLowerCase() === input.expectedAccount.toLowerCase()));
            if (input.expectedAccount && !selected.length && candidates.length) fail('ACCOUNT_MISMATCH', 'The open Teams session does not match TEAMS_ACCOUNT.');
            if (input.tenantId) selected = selected.filter(x => x.claims.tid === input.tenantId);
            if (!(input.expectedAccount && input.tenantId) && active.length) selected = selected.filter(x => active.some(a => a.localAccountId === x.claims.oid && a.tenantId === x.claims.tid));
            const identities = new Map(selected.map(x => [`${x.claims.oid}:${x.claims.tid}`, x]));
            if (identities.size > 1) fail('ACCOUNT_AMBIGUOUS', 'Select TEAMS_ACCOUNT and TEAMS_TENANT_ID to identify one Teams account.');
            if (!identities.size) fail('AUTH_REQUIRED', 'Open Teams in this browser and finish signing in, then retry. The connector never starts a separate login.');
            const chosen = [...identities.values()][0];
            const claim = chosen.claims;
            const profile = {id:claim.oid, mri:`8:orgid:${claim.oid}`, tenantId:claim.tid, email:appAlias(claim) || email(claim), displayName:claim.name || ''};
            const own = tokens.filter(entry => {
                const p = payload(entry.secret);
                return entry.credentialType === 'AccessToken' && (entry.tokenType || 'Bearer') === 'Bearer'
                    && ((p.oid === claim.oid && p.tid === claim.tid) ||
                    (entry.homeAccountId && entry.homeAccountId === chosen.entry.homeAccountId && entry.realm === claim.tid));
            }).sort((a,b) => Number(b.expiresOn || payload(b.secret).exp) - Number(a.expiresOn || payload(a.secret).exp));
            const find = predicate => own.find(e => predicate(e, payload(e.secret)))?.secret;
            return {profile, search:find((e,p) => e.target?.includes('SubstrateSearch') && p.aud === 'https://substrate.office.com') || find(e => e.target?.includes('SubstrateSearch')),
                spaces:find((e,p) => p.aud === 'https://api.spaces.skype.com'), csa:find((e,p) => p.aud === 'https://chatsvcagg.teams.microsoft.com')};
        }

        const shapes = {
            substrate: {method:'POST', path:/^\/(?:searchservice\/api\/v2\/query|search\/api\/v1\/suggestions)$/, base:'https://substrate.office.com'},
            chat: {method:'GET', path:/^\/v1\/(?:users\/ME\/conversations(?:\/[^/]+(?:\/messages(?:\/[^/]+)?)?)?|threads\/[^/]+\/consumptionhorizons)$/, key:'chatServiceAfd', root:/^\/api\/chatsvc\/[a-z]+$/},
            csa: {method:'GET', path:/^\/api\/(?:v3\/teams\/users\/me|v1\/teams\/users\/me\/conversationFolders)$/, key:'chatSvcAggAfd', root:/^\/api\/csa\/[a-z]+$/},
            profile: {method:'POST', path:/^\/beta\/users\/fetchShortProfile$/, key:'middleTier', root:/^\/api\/mt\/(?:part\/)?[a-z]+(?:-\d+)?$/}
        };

        function trustedRoot(base, shape) {
            let root;
            try { root = new URL(base); } catch { return false; }
            return origins.includes(root.origin) && !root.username && !root.password && !root.search && !root.hash && shape.root.test(root.pathname);
        }

        async function endpoint(request, view, auth) {
            const shape = shapes[request?.service];
            if (!shape || shape.method !== request.method || !shape.path.test(request.path)) fail('UNSAFE_ENDPOINT', 'Unsupported Teams read endpoint.');
            let base = shape.base;
            if (!base) {
                const keys = view.keys.filter(k => k.includes('DISCOVER-REGION-GTM'));
                const scoped = keys.filter(k => k.includes(auth.profile.id));
                const configs = [...new Set((scoped.length ? scoped : keys).map(k => parse(view.read(k))?.item?.[shape.key]).filter(Boolean))];
                base = configs.length === 1 ? configs[0] : null;
                if (!base) {
                    // Teams' own token exchange reports the same regional roots when the cached discovery is missing or ambiguous.
                    const fallback = auth.spaces && (await exchange(auth).catch(() => null))?.regions?.[shape.key];
                    if (typeof fallback === 'string' && (!configs.length || configs.includes(fallback))) base = fallback;
                    else if (configs.some(config => !trustedRoot(config, shape))) fail('UNSAFE_ENDPOINT', 'Teams discovery returned an unexpected endpoint.');
                    else fail('REGION_UNAVAILABLE', 'Teams region discovery is unavailable or ambiguous. Finish loading Teams and retry.');
                }
                if (!trustedRoot(base, shape)) fail('UNSAFE_ENDPOINT', 'Teams discovery returned an unexpected endpoint.');
            }
            const url = new URL(base + request.path);
            if (url.href !== base + request.path) fail('UNSAFE_ENDPOINT', 'The request contains an invalid path.');
            for (const [key,value] of request.query || []) url.searchParams.append(key,value);
            return url.href;
        }

        function retryDelay(response) {
            if (response.status >= 500) return 400;
            if (response.status !== 429) return null;
            const header = response.headers?.get?.('Retry-After');
            if (!header) return 1000;
            const seconds = Number(header);
            const delay = Number.isFinite(seconds) ? seconds * 1000 : Date.parse(header) - Date.now();
            return Number.isFinite(delay) && delay <= 5000 ? Math.max(0, delay) : null;
        }

        async function fetchJson(url, options, retried = false) {
            let response;
            try { response = await fetch(url, {...options, credentials:'omit', redirect:'error', signal:AbortSignal.timeout(15000)}); }
            catch {
                if (!retried) { await sleep(400); return fetchJson(url, options, true); }
                fail('NETWORK_ERROR', 'Teams request failed or timed out. Check the open Teams page and connection, then retry.');
            }
            if (!response.ok) {
                const delay = retried ? null : retryDelay(response);
                if (delay !== null) {
                    try { await response.body?.cancel(); } catch { /* The retry does not depend on the discarded body. */ }
                    await sleep(delay);
                    return fetchJson(url, options, true);
                }
                if (response.status === 400 && new URL(url).pathname === '/search/api/v1/suggestions') {
                    const body = await responseText(response, 16384);
                    if (body.includes('No providers were enabled') && body.includes('GuestAccess: True')) {
                        fail('FEATURE_UNAVAILABLE', 'Teams people search is unavailable for this guest account. Use a known person ID or search in the Teams interface.');
                    }
                }
                const code = response.status === 401 ? 'AUTH_EXPIRED' : response.status === 403 ? 'ACCESS_DENIED' : response.status === 429 ? 'RATE_LIMITED' : 'API_ERROR';
                fail(code, `Teams returned HTTP ${response.status}. ${response.status === 401 ? 'Use the open Teams page to renew the session.' : 'Retry later or check access in Teams.'}`);
            }
            const result = parse(await responseText(response));
            if (!result || typeof result !== 'object') fail('INVALID_RESPONSE', 'Teams returned an unsupported response.');
            return result;
        }

        async function responseText(response, maxBytes = 2 * 1024 * 1024) {
            const reader = response.body.getReader();
            let bytes = 0, text = '', decoder = new TextDecoder();
            try {
                while (true) {
                    const {value,done} = await reader.read();
                    if (done) break;
                    bytes += value.byteLength;
                    if (bytes > maxBytes) { await reader.cancel(); fail('RESPONSE_TOO_LARGE', 'Teams response exceeds the size limit. Use a smaller page.'); }
                    text += decoder.decode(value, {stream:true});
                }
            } finally { reader.releaseLock(); }
            return text + decoder.decode();
        }

        async function exchange(auth) {
            // One Skype token per account and tenant, reused until shortly before Teams says it expires.
            const key = `${auth.profile.id}:${auth.profile.tenantId}`;
            const cached = skypeCache.get(key);
            if (cached?.token && cached.expiresAt > Date.now() + 60000) return cached;
            if (cached?.pending) return cached.pending;
            if (!auth.spaces) fail('AUTH_REQUIRED', 'The open Teams session has no valid Skype Spaces access token. Reload Teams and retry.');
            const pending = (async () => {
                const data = await fetchJson('https://authsvc.teams.microsoft.com/v1.0/authz', {
                    method:'POST', headers:{Authorization:`Bearer ${auth.spaces}`, 'Content-Type':'application/json'},
                    body:JSON.stringify({})
                });
                const token = data.tokens?.skypeToken;
                if (typeof token !== 'string' || !token) fail('AUTH_REQUIRED', 'Teams could not supply its chat session token.');
                const lifetime = Number(data.tokens?.expiresIn);
                const seconds = Number.isFinite(lifetime) && lifetime > 0 ? Math.min(lifetime, 86400) : 900;
                const regions = data.regionGtms && typeof data.regionGtms === 'object' ? data.regionGtms : {};
                return {token, regions, expiresAt: Date.now() + seconds * 1000};
            })();
            skypeCache.set(key, {pending});
            try {
                const entry = await pending;
                skypeCache.set(key, entry);
                return entry;
            } catch (error) {
                skypeCache.delete(key);
                throw error;
            }
        }

        async function perform(request, view, auth) {
            const url = await endpoint(request, view, auth);
            const bearer = request.service === 'substrate' ? auth.search : request.service === 'csa' ? auth.csa : auth.spaces;
            if (!bearer) fail('AUTH_REQUIRED', 'The open Teams session is missing an access token for this operation. Use Teams and retry.');
            let body = request.body;
            if (request.service === 'substrate') {
                body = {...body, cvid:crypto.randomUUID(), logicalId:crypto.randomUUID()};
                if (request.path === '/searchservice/api/v2/query') body.timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
            }
            const send = async () => {
                const headers = {Authorization:`Bearer ${bearer}`, Accept:'application/json', 'Content-Type':'application/json'};
                if (request.service !== 'substrate') headers.Authentication = `skypetoken=${(await exchange(auth)).token}`;
                return fetchJson(url, {method:request.method, headers, ...(body != null ? {body:JSON.stringify(body)} : {})});
            };
            try { return await send(); }
            catch (error) {
                if (!failures.has(error) || error.code !== 'AUTH_EXPIRED' || request.service === 'substrate') throw error;
                skypeCache.delete(`${auth.profile.id}:${auth.profile.tenantId}`);
                return send();
            }
        }

        async function batch(requests, view, auth) {
            if (!Array.isArray(requests) || !requests.length || requests.length > 20) fail('INVALID_INPUT', 'A batch requires 1–20 requests.');
            const results = new Array(requests.length);
            let next = 0;
            const worker = async () => {
                while (next < requests.length) {
                    const index = next++;
                    try { results[index] = {ok:true, data: await perform(requests[index], view, auth)}; }
                    catch (error) { results[index] = {ok:false, error: failures.has(error) ? {code:error.code, message:error.message} : {code:'SESSION_UNAVAILABLE', message:'Teams session data is unavailable.'}}; }
                }
            };
            await Promise.all([worker(), worker(), worker(), worker()]);
            if (JSON.stringify(results).length > 3.5 * 1024 * 1024) fail('RESPONSE_TOO_LARGE', 'Teams responses exceed the size limit. Use a smaller page.');
            return results;
        }

        try {
            if (!origins.includes(location.origin)) fail('WRONG_ORIGIN', 'The selected tab is no longer a supported Teams page.');
            const view = storageView();
            const tokens = [...await readTokens(view), ...await readTeamsTokens(view)];
            const auth = selectAccount(tokens, view);
            if (input.action === 'status' || input.action === 'profile') {
                return {ok:true, profile:auth.profile, capabilities:{search:!!auth.search, chat:!!auth.spaces, channels:!!auth.csa && !!auth.spaces}};
            }
            if (input.action === 'lease') {
                // Lease transport only: the selected account's short-lived access tokens and
                // their expiry. Refresh tokens and cookies never leave the page.
                const grant = token => token ? {token, expiresAt:Number(payload(token).exp) || 0} : null;
                return {ok:true, profile:auth.profile, lease:{spaces:grant(auth.spaces), search:grant(auth.search), csa:grant(auth.csa)}};
            }
            if (input.action === 'batch') return {ok:true, profile:auth.profile, data: await batch(input.requests, view, auth)};
            if (input.action !== 'request') fail('INVALID_INPUT', 'Unsupported bridge operation.');
            return {ok:true, profile:auth.profile, data: await perform(input.request, view, auth)};
        } catch (error) {
            // Never return exception text: browser/network errors may contain credentials or private content.
            return {ok:false, error:failures.has(error) ? {code:error.code,message:error.message} : {code:'SESSION_UNAVAILABLE', message:'Teams session data is unavailable. Finish loading or signing in to Teams, then retry.'}};
        }
    };
})()
