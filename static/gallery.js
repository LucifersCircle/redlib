(function() {
    function initializeGallery(gallery) {
        const track = gallery.querySelector('[data-gallery-track]');
        const slides = Array.from(gallery.querySelectorAll('[data-gallery-slide]'));
        const current = gallery.querySelector('[data-gallery-current]');
        if (!track || slides.length < 2 || !current) return;

        let scrollFrame = 0;
        let pointerStart = null;
        let suppressClick = false;

        function updateCurrentSlide() {
            scrollFrame = 0;
            let closestIndex = 0;
            let closestDistance = Infinity;

            slides.forEach(function(slide, index) {
                const distance = Math.abs(slide.offsetLeft - track.scrollLeft);
                if (distance < closestDistance) {
                    closestDistance = distance;
                    closestIndex = index;
                }
            });

            current.textContent = String(closestIndex + 1);
        }

        function scheduleCurrentSlideUpdate() {
            if (scrollFrame === 0) {
                scrollFrame = window.requestAnimationFrame(updateCurrentSlide);
            }
        }

        track.addEventListener('scroll', scheduleCurrentSlideUpdate, { passive: true });
        window.addEventListener('resize', scheduleCurrentSlideUpdate, { passive: true });

        track.addEventListener('pointerdown', function(event) {
            if (!event.isPrimary) return;
            pointerStart = { x: event.clientX, y: event.clientY };
            suppressClick = false;
        });

        track.addEventListener('pointermove', function(event) {
            if (!pointerStart || !event.isPrimary) return;
            if (Math.hypot(event.clientX - pointerStart.x, event.clientY - pointerStart.y) > 8) {
                suppressClick = true;
            }
        }, { passive: true });

        track.addEventListener('pointerup', function() {
            pointerStart = null;
        });

        track.addEventListener('pointercancel', function() {
            pointerStart = null;
        });

        track.addEventListener('click', function(event) {
            if (!suppressClick) return;
            event.preventDefault();
            event.stopPropagation();
            suppressClick = false;
        }, true);

        updateCurrentSlide();
    }

    function initializeGalleries() {
        document.querySelectorAll('[data-gallery]').forEach(initializeGallery);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', initializeGalleries);
    } else {
        initializeGalleries();
    }
})();
