(function() {
    const touchPointer = window.matchMedia('(hover: none), (pointer: coarse)');
    let recentTouch = 0;

    document.addEventListener('pointerdown', function(event) {
        if (event.pointerType === 'touch' || event.pointerType === 'pen') {
            recentTouch = Date.now();
        }
    }, { passive: true });

    document.addEventListener('touchstart', function() {
        recentTouch = Date.now();
    }, { passive: true });

    document.addEventListener('click', function(event) {
        // Keyboard-generated clicks have detail=0. Keep the existing keyboard
        // behavior unchanged; this enhancement is only for taps.
        const isTap = event.detail !== 0 && (touchPointer.matches || Date.now() - recentTouch < 1000);
        if (!isTap) return;

        const target = event.target;
        if (!(target instanceof Element)) return;

        const spoiler = target.closest('.md-spoiler-text');
        if (!spoiler || spoiler.classList.contains('spoiler_revealed')) return;

        // The first tap reveals the entire spoiler. If the tap was on a link,
        // cancelling it means the second tap can follow the now-visible link.
        event.preventDefault();
        event.stopPropagation();
        spoiler.classList.add('spoiler_revealed');
    }, true);
})();
