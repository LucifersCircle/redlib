(function() {
    const touchPointer = window.matchMedia('(hover: none), (pointer: coarse)');
    let recentTouch = { time: 0, moved: false, x: 0, y: 0 };

    document.addEventListener('pointerdown', function(event) {
        if (event.pointerType === 'touch' || event.pointerType === 'pen') {
            recentTouch = { time: Date.now(), moved: false, x: event.clientX, y: event.clientY };
        }
    }, { passive: true });

    document.addEventListener('pointermove', function(event) {
        if (event.pointerType !== 'touch' && event.pointerType !== 'pen') return;
        if (Math.hypot(event.clientX - recentTouch.x, event.clientY - recentTouch.y) > 8) {
            recentTouch.moved = true;
        }
    }, { passive: true });

    document.addEventListener('touchstart', function(event) {
        const touch = event.touches[0];
        if (touch) {
            recentTouch = { time: Date.now(), moved: false, x: touch.clientX, y: touch.clientY };
        }
    }, { passive: true });

    document.addEventListener('touchmove', function(event) {
        const touch = event.touches[0];
        if (touch && Math.hypot(touch.clientX - recentTouch.x, touch.clientY - recentTouch.y) > 8) {
            recentTouch.moved = true;
        }
    }, { passive: true });

    document.addEventListener('click', function(event) {
        // Keyboard-generated clicks have detail=0. Keep the existing keyboard
        // behavior unchanged. Pointer clicks, including mouse clicks, make
        // the reveal persistent instead of relying only on transient :hover.
        const pointerType = 'pointerType' in event ? event.pointerType : '';
        const isExplicitTap = pointerType === 'touch' || pointerType === 'pen';
        const isFallbackTap = pointerType === '' && (touchPointer.matches || Date.now() - recentTouch.time < 1000);
        const isTap = isExplicitTap || isFallbackTap;
        if (event.detail === 0 || event.button > 0 || (isTap && recentTouch.moved)) return;

        const target = event.target;
        if (!(target instanceof Element)) return;

        let spoiler = target.closest('.md-spoiler-text');
        if (!spoiler) return;

        const spoilersToReveal = [];
        while (spoiler) {
            if (!spoiler.classList.contains('spoiler_revealed')) {
                spoilersToReveal.push(spoiler);
            }
            spoiler = spoiler.parentElement ? spoiler.parentElement.closest('.md-spoiler-text') : null;
        }
        if (spoilersToReveal.length === 0) return;

        // Touch has no reliable hover preview: its first tap reveals and the
        // second can navigate. Mouse users already see links on hover, so keep
        // their normal link activation while remembering the revealed state.
        if (isTap) {
            event.preventDefault();
            event.stopPropagation();
        }
        spoilersToReveal.forEach(function(item) {
            item.classList.add('spoiler_revealed');
            item.querySelectorAll('.md-spoiler-text').forEach(function(nested) {
                nested.classList.add('spoiler_revealed');
            });
        });
    }, true);
})();
