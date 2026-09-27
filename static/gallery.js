(function() {
    function initializeGallery(gallery) {
        const track = gallery.querySelector('[data-gallery-track]');
        const slides = Array.from(gallery.querySelectorAll('[data-gallery-slide]'));
        const current = gallery.querySelector('[data-gallery-current]');
        const previous = gallery.querySelector('[data-gallery-previous]');
        const next = gallery.querySelector('[data-gallery-next]');
        if (!track || slides.length < 2 || !current) return;

        let scrollFrame = 0;
        let pointerStart = null;
        let activePointerId = null;
        let pointerMoved = false;
        let suppressClickUntil = 0;

        function loadSlide(index) {
            const slide = slides[index];
            if (!slide) return;

            const image = slide.querySelector('img[data-src]');
            if (image) {
                image.src = image.dataset.src;
                image.removeAttribute('data-src');
            }
        }

        function loadAdjacentSlides(index) {
            loadSlide(index - 1);
            loadSlide(index);
            loadSlide(index + 1);
        }

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
            loadAdjacentSlides(closestIndex);
            if (previous) previous.disabled = closestIndex === 0;
            if (next) next.disabled = closestIndex === slides.length - 1;
        }

        function scheduleCurrentSlideUpdate() {
            if (scrollFrame === 0) {
                scrollFrame = window.requestAnimationFrame(updateCurrentSlide);
            }
        }

        track.addEventListener('scroll', scheduleCurrentSlideUpdate, { passive: true });
        window.addEventListener('resize', scheduleCurrentSlideUpdate, { passive: true });

        function showSlide(index) {
            const slide = slides[index];
            if (!slide) return;
            loadAdjacentSlides(index);
            track.scrollTo({ left: slide.offsetLeft, behavior: 'smooth' });
        }

        if (previous) {
            previous.addEventListener('click', function() {
                showSlide(Math.max(0, Number(current.textContent) - 2));
            });
        }

        if (next) {
            next.addEventListener('click', function() {
                showSlide(Math.min(slides.length - 1, Number(current.textContent)));
            });
        }

        track.addEventListener('pointerdown', function(event) {
            if (!event.isPrimary) return;
            pointerStart = { x: event.clientX, y: event.clientY };
            activePointerId = event.pointerId;
            pointerMoved = false;
            suppressClickUntil = 0;
        });

        window.addEventListener('pointermove', function(event) {
            if (!pointerStart || event.pointerId !== activePointerId) return;
            if (Math.hypot(event.clientX - pointerStart.x, event.clientY - pointerStart.y) > 8) {
                pointerMoved = true;
            }
        }, { passive: true });

        window.addEventListener('pointerup', function(event) {
            if (event.pointerId !== activePointerId) return;
            if (pointerMoved) suppressClickUntil = Date.now() + 500;
            pointerStart = null;
            activePointerId = null;
            pointerMoved = false;
        });

        window.addEventListener('pointercancel', function(event) {
            if (event.pointerId !== activePointerId) return;
            pointerStart = null;
            activePointerId = null;
            pointerMoved = false;
            suppressClickUntil = 0;
        });

        track.addEventListener('click', function(event) {
            if (event.detail === 0 || Date.now() > suppressClickUntil) return;
            event.preventDefault();
            event.stopPropagation();
            suppressClickUntil = 0;
        }, true);

        updateCurrentSlide();
    }

    function initializeGalleries() {
        document.body.classList.add('gallery-js');
        document.querySelectorAll('[data-gallery]').forEach(initializeGallery);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', initializeGalleries);
    } else {
        initializeGalleries();
    }
})();
