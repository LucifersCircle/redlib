(function() {
    let collapseGeneration = 0;

    document.addEventListener('click', function(event) {
        if (event.defaultPrevented || event.button > 0 || event.detail === 0) return;
        if (!(event.target instanceof Element)) return;

        const rail = event.target.closest('.comment_collapse');
        if (!rail) return;

        // The rail belongs to the native summary, even when it stretches many
        // screens down alongside replies. Never select an ancestor's summary.
        const summary = rail.parentElement;
        const details = summary && summary.parentElement;
        if (!summary || summary.tagName !== 'SUMMARY' || !summary.classList.contains('comment_data')) return;
        if (!details || details.tagName !== 'DETAILS' || !details.classList.contains('comment_right') || !details.open) return;

        const previousTop = summary.getBoundingClientRect().top;
        const generation = ++collapseGeneration;

        // Observe, rather than replace, native activation. Measure after it has
        // closed and after the browser has adjusted/clamped its scroll position.
        window.requestAnimationFrame(function() {
            if (generation !== collapseGeneration || event.defaultPrevented || details.open || !details.isConnected) return;
            if (summary.getClientRects().length === 0) return;

            const viewportTop = window.visualViewport ? window.visualViewport.offsetTop : 0;
            const navbar = document.querySelector('nav.fixed_navbar');
            const navbarBottom = navbar ? navbar.getBoundingClientRect().bottom : 0;
            const safeTop = Math.max(viewportTop, navbarBottom) + 8;
            // Keep an already-visible heading in place; recover an offscreen
            // parent below the navbar so its actual next comment follows it.
            const targetTop = Math.max(previousTop, safeTop);
            const correction = summary.getBoundingClientRect().top - targetTop;
            if (Math.abs(correction) < 1) return;

            // Immediate compensation avoids travelling through removed content,
            // preserves horizontal position, and introduces no focus movement.
            window.scrollTo(window.scrollX, window.scrollY + correction);
        });
    }, true);
})();
