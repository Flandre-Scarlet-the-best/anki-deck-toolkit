(() => {
    const status=performance.getEntriesByType('navigation')[0]?.responseStatus || 0;
    const sameOrigin=location.origin==='https://forvo.com';
    const challenge=!!document.querySelector('script[src*="/cdn-cgi/challenge-platform"],iframe[src*="challenges.cloudflare.com"],#challenge-running,#challenge-stage');
    const logout=Array.from(document.querySelectorAll('#nav_account .sign_out a[href]')).some(a=>new URL(a.href).origin===location.origin&&new URL(a.href).pathname==='/logout/');
    const login=!!document.querySelector('#header .login,form input[type="password"]') || Array.from(document.querySelectorAll('#header a[href]')).some(a=>new URL(a.href).pathname==='/login/');
    return {same_origin:sameOrigin,status,challenge,logout,login,ready:document.readyState!=='loading'&&!!document.body};
})()
