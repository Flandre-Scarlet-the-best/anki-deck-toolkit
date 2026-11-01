// Адаптер проверенной разметки обычной страницы Forvo. Аудиоадреса из Play() не используются.
((surface, action, recordId) => {
    const fail = code => ({code, candidates: []});
    const wordPath = '/word/' + encodeURIComponent(surface) + '/';
    if (location.origin !== 'https://forvo.com' || decodeURIComponent(location.pathname) !== decodeURIComponent(wordPath)) return fail('page_identity_mismatch');
    if (!document.querySelector('.page_word')) return fail('dom_changed');
    const containers = document.querySelectorAll('#language-container-ja');
    if (containers.length === 0) {
        return fail(document.querySelector('.language-container .header-pronunciation a[href$="/languages/"]') ? 'dom_changed' :
            document.querySelector('.language-container .header-pronunciation') ? 'japanese_pronunciation_not_found' : 'dom_changed');
    }
    if (containers.length !== 1) return fail('dom_changed');
    const container = containers[0];
    const article = container.querySelector(':scope > article#language-ja.pronunciations');
    const list = article?.querySelector(':scope > ul#pronunciations-list-ja.pronunciations-list');
    const languageLink = article?.querySelector(':scope > header.header-pronunciation a[href]');
    if (!list || !languageLink || new URL(languageLink.href).origin !== location.origin || new URL(languageLink.href).pathname !== '/languages/ja/') return fail('dom_changed');
    if (action === 'expand') {
        const more = article.querySelector(':scope > #show-more-pronunciations-ja.active');
        if (more) more.click();
    }
    const candidates = [];
    for (const row of list.querySelectorAll(':scope > li.pronunciation')) {
        const plays = row.querySelectorAll(':scope > .play[id^="play_"]');
        if (plays.length !== 1 || !/^play_[1-9][0-9]*$/.test(plays[0].id)) return fail('dom_changed');
        const id = plays[0].id.slice(5);
        const buttons = row.querySelectorAll('.download > .ofLink, .download > a');
        let button = null;
        for (const b of buttons) {
            // Это реальные атрибуты обычной кнопки, а не выведенные из порядка DOM.
            let word; try {word = decodeURIComponent(b.getAttribute('data-p2') || '');} catch {return fail('dom_changed');}
            if (word === surface && b.getAttribute('data-p3') === 'ja' && b.getAttribute('data-p4') === id) {
                if (button) return fail('dom_changed');
                button = b;
            }
        }
        if (buttons.length && !button) return fail('page_identity_mismatch');
        const username = row.querySelector(':scope > .info [data-p2]')?.getAttribute('data-p2') || null;
        const rawVotes = row.querySelector('#word_rate_' + id + '.votes_ja_content .num_votes')?.textContent.trim() || '';
        const voteMatch = rawVotes.match(/^(-?\d+)(?:\s|$)/);
        const votes = voteMatch && Number.isSafeInteger(Number(voteMatch[1])) ? Number(voteMatch[1]) : null;
        const from = row.querySelector(':scope > .from')?.textContent.trim().replace(/^\(|\)$/g, '') || '';
        // Адаптер использует англоязычную версию forvo.com; неизвестные формы не угадываются.
        const speaker = from.match(/^(Female|Male) from (.+)$/);
        const candidate = {record_id:id, username, votes, gender:speaker ? speaker[1].toLowerCase() : null,
            country:speaker ? speaker[2] : null, download_available:!!button};
        candidates.push(candidate);
        if ((action === 'click' || action === 'activate') && id === recordId) {
            if (!button) return fail('download_button_unavailable');
            row.querySelector('.pronunciationMoreMenu')?.click();
            button.scrollIntoView({block:'center',inline:'center',behavior:'instant'});
            const box=button.getBoundingClientRect();
            const point={x:box.x+box.width/2,y:box.y+box.height/2};
            if (box.width<=0 || box.height<=0 || point.x<0 || point.y<0 || point.x>=innerWidth || point.y>=innerHeight) return fail('download_button_unavailable');
            const hit=document.elementFromPoint(point.x,point.y);
            if (hit!==button && !button.contains(hit)) return fail('download_button_obscured');
            // Расширение не имеет доверенного ввода CDP: обычный click() той же проверенной кнопки.
            if (action === 'activate') {button.click(); return {code:'activated', candidates:[candidate]};}
            return {code:'prepared', candidates:[candidate], point};
        }
    }
    if (action === 'click' || action === 'activate') return fail('candidate_disappeared');
    return {code:candidates.length ? 'ok' : 'no_usable_candidates', candidates};
})
