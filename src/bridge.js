async function teamsBridge(input) {
    // Runs only inside the selected Teams page. No credential leaves this function.
    const origins = ['https://teams.microsoft.com', 'https://teams.cloud.microsoft'];
    const failures = new WeakSet();
    const fail = (code, message) => { const error = {code, message}; failures.add(error); throw error; };
    const decode = value => Uint8Array.from(atob(value.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
    const parse = value => { try { return JSON.parse(value); } catch { return null; } };
    const payload = value => { try { return parse(new TextDecoder().decode(decode(value.split('.')[1]))) || {}; } catch { return {}; } };
    const email = claims => claims.preferred_username || claims.upn || claims.email || '';
    const valid = entry => Number(entry.expiresOn || payload(entry.secret).exp) > Date.now() / 1000 + 30;

    async function readTokens(storage, keys) {
        const cookieText = document.cookie.split(';').map(s => s.trim()).find(s => s.startsWith('msal.cache.encryption='));
        const cookie = cookieText && parse(decodeURIComponent(cookieText.slice(cookieText.indexOf('=') + 1)));
        const base = cookie?.key && await crypto.subtle.importKey('raw', decode(cookie.key), 'HKDF', false, ['deriveKey']);
        const clients = keys.filter(k => /^msal\.(?:\d+\.)?token\.keys\./.test(k)).map(k => k.split('.').at(-1));
        const entries = [];
        for (const key of keys.filter(k => /(?:access|id)token/i.test(k))) {
            let entry = parse(storage[key]);
            if (entry?.id && entry.nonce && entry.data) {
                if (!base || entry.id !== cookie.id) continue;
                const legacyClient = key.match(/-(?:access|id|refresh)token-([a-f\d-]{36})-/i)?.[1];
                entry = await decrypt(entry, base, [...clients.filter(c => key.includes(c)), legacyClient, ''].filter(c => c !== undefined));
            }
            if (entry?.secret && (['AccessToken', 'IdToken'].includes(entry.credentialType) && valid(entry))) entries.push(entry);
        }
        return entries;
    }

    async function readTeamsTokens(storage) {
        let exported;
        if (input.nativeApp) {
            const auth = globalThis.nativeModules?.Auth;
            if (typeof auth?.acquireEncryptionKey !== 'function') fail('APP_BRIDGE_UNAVAILABLE', 'The selected Teams app window has not loaded its authentication bridge. Select the main Teams window and retry.');
            exported = (await auth.acquireEncryptionKey({correlationId:crypto.randomUUID()}))?.result;
            if (typeof exported !== 'string' || !exported) fail('APP_BRIDGE_UNAVAILABLE', 'The Teams app could not provide its session encryption key. Finish signing in inside the app and retry.');
        } else {
            exported = parse(storage['tmp.auth.v1.GLOBAL.ExportedEncryptionKey.ExportedEncryptionKey'])?.item?.exportedKey;
        }
        const key = exported && await crypto.subtle.importKey('raw', decode(exported), 'AES-CBC', false, ['decrypt']);
        const entries = [];
        for (const name of Object.keys(storage).filter(k => /\.Token\.HTTPS:\/\/(?:API\.SPACES\.SKYPE\.COM|CHATSVCAGG\.TEAMS\.MICROSOFT\.COM|SUBSTRATE\.OFFICE\.COM|OUTLOOK\.OFFICE\.COM)/.test(k))) {
            try {
                const item = parse(storage[name])?.item;
                let secret = item?.token;
                if (!secret && key && item?.encryptedToken && item.iv) {
                    secret = new TextDecoder().decode(await crypto.subtle.decrypt({name:'AES-CBC', iv:decode(item.iv)}, key, decode(item.encryptedToken)));
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

    function selectAccount(tokens, storage) {
        const candidates = tokens.map(entry => ({entry, claims:payload(entry.secret)})).filter(x => x.claims.oid && x.claims.tid);
        const appUser = input.nativeApp && parse(storage['tmp.auth.v1.GLOBAL.User.User'])?.item;
        const appProfile = appUser?.profile;
        const hasAppIdentity = appProfile?.oid && appProfile?.tid;
        if (input.nativeApp && !hasAppIdentity && !(input.expectedAccount && input.tenantId)) fail('ACCOUNT_SELECTION_REQUIRED', 'The Teams app active account cannot be established. Set TEAMS_ACCOUNT and TEAMS_TENANT_ID explicitly.');
        const active = hasAppIdentity ? [{localAccountId:appProfile.oid,tenantId:appProfile.tid}]
            : Object.keys(storage).filter(k => k.endsWith('.active-account-filters')).map(k => parse(storage[k])).filter(Boolean);
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

    function endpoint(request, storage, profile) {
        const shapes = {
            substrate: {method:'POST', path:/^\/(?:searchservice\/api\/v2\/query|search\/api\/v1\/suggestions)$/, base:'https://substrate.office.com'},
            chat: {method:'GET', path:/^\/v1\/(?:users\/ME\/conversations(?:\/[^/]+(?:\/messages(?:\/[^/]+)?)?)?|threads\/[^/]+\/consumptionhorizons)$/, key:'chatServiceAfd', root:/^\/api\/chatsvc\/[a-z]+$/},
            csa: {method:'GET', path:/^\/api\/v3\/teams\/users\/me$/, key:'chatSvcAggAfd', root:/^\/api\/csa\/[a-z]+$/},
            profile: {method:'POST', path:/^\/beta\/users\/fetchShortProfile$/, key:'middleTier', root:/^\/api\/mt\/(?:part\/)?[a-z]+(?:-\d+)?$/}
        };
        const shape = shapes[request?.service];
        if (!shape || shape.method !== request.method || !shape.path.test(request.path)) fail('UNSAFE_ENDPOINT', 'Unsupported Teams read endpoint.');
        let base = shape.base;
        if (!base) {
            const keys = Object.keys(storage).filter(k => k.includes('DISCOVER-REGION-GTM'));
            const scoped = keys.filter(k => k.includes(profile.id));
            const configs = (scoped.length ? scoped : keys).map(k => parse(storage[k])?.item?.[shape.key]).filter(Boolean);
            const unique = [...new Set(configs)];
            if (unique.length !== 1) fail('REGION_UNAVAILABLE', 'Teams region discovery is unavailable or ambiguous. Finish loading Teams and retry.');
            base = unique[0];
            const root = new URL(base);
            if (!origins.includes(root.origin) || root.username || root.password || root.search || root.hash || !shape.root.test(root.pathname)) fail('UNSAFE_ENDPOINT', 'Teams discovery returned an unexpected endpoint.');
        }
        const url = new URL(base + request.path);
        if (url.href !== base + request.path) fail('UNSAFE_ENDPOINT', 'The request contains an invalid path.');
        for (const [key,value] of request.query || []) url.searchParams.append(key,value);
        return url.href;
    }

    async function fetchJson(url, options) {
        let response;
        try { response = await fetch(url, {...options, credentials:'omit', redirect:'error', signal:AbortSignal.timeout(15000)}); }
        catch { fail('NETWORK_ERROR', 'Teams request failed or timed out. Check the open Teams page and connection, then retry.'); }
        if (!response.ok) {
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

    async function skypeToken(auth) {
        if (!auth.spaces) fail('AUTH_REQUIRED', 'The open Teams session has no valid Skype Spaces access token. Reload Teams and retry.');
        const data = await fetchJson('https://authsvc.teams.microsoft.com/v1.0/authz', {
            method:'POST', headers:{Authorization:`Bearer ${auth.spaces}`, 'Content-Type':'application/json'},
            body:JSON.stringify({})
        });
        const token = data.tokens?.skypeToken;
        if (typeof token !== 'string' || !token) fail('AUTH_REQUIRED', 'Teams could not supply its chat session token.');
        return token;
    }


    try {
        if (!origins.includes(location.origin)) fail('WRONG_ORIGIN', 'The selected tab is no longer a supported Teams page.');
        const storage = {...localStorage, ...sessionStorage};
        const tokens = [...await readTokens(storage, Object.keys(storage)), ...await readTeamsTokens(storage)];
        const auth = selectAccount(tokens, storage);
        if (input.action === 'status' || input.action === 'profile') {
            return {ok:true, profile:auth.profile, capabilities:{search:!!auth.search, chat:!!auth.spaces, channels:!!auth.csa && !!auth.spaces}};
        }
        if (input.action !== 'request') fail('INVALID_INPUT', 'Unsupported bridge operation.');
        const request = input.request, url = endpoint(request, storage, auth.profile);
        const bearer = request.service === 'substrate' ? auth.search : request.service === 'csa' ? auth.csa : auth.spaces;
        if (!bearer) fail('AUTH_REQUIRED', 'The open Teams session is missing an access token for this operation. Use Teams and retry.');
        if (request.service === 'substrate') {
            request.body.cvid = crypto.randomUUID();
            request.body.logicalId = crypto.randomUUID();
            if (request.path === '/searchservice/api/v2/query') request.body.timezone = Intl.DateTimeFormat().resolvedOptions().timeZone;
        }
        const headers = {Authorization:`Bearer ${bearer}`, Accept:'application/json', 'Content-Type':'application/json'};
        if (request.service !== 'substrate') headers.Authentication = `skypetoken=${await skypeToken(auth)}`;
        const data = await fetchJson(url, {method:request.method, headers, ...(request.body != null ? {body:JSON.stringify(request.body)} : {})});
        return {ok:true, profile:auth.profile, data};
    } catch (error) {
        // Never return exception text: browser/network errors may contain credentials or private content.
        return {ok:false, error:failures.has(error) ? {code:error.code,message:error.message} : {code:'SESSION_UNAVAILABLE', message:'Teams session data is unavailable. Finish loading or signing in to Teams, then retry.'}};
    }
}
