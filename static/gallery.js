(function() {
    function prepareVideo(video) {
        if (video.dataset.poster) {
            video.poster = video.dataset.poster;
            video.removeAttribute('data-poster');
        }
    }

    function loadVideo(video) {
        prepareVideo(video);
        if (!video.dataset.src) return;
        video.preload = 'metadata';
        video.src = video.dataset.src;
        video.removeAttribute('data-src');
        if (typeof video.load === 'function') video.load();
    }

    function pauseVideo(video) {
        video.dataset.playRequested = 'false';
        delete video.dataset.userGestureAt;
        if (typeof video.pause === 'function' && video.paused !== true) {
            video.dataset.managedPause = 'true';
            video.pause();
        }
    }

    function playVideo(video) {
        if (video.dataset.fallbackShown === 'true') return;
        loadVideo(video);
        if (video.dataset.autoplay === 'false' || video.dataset.userPaused === 'true') return;
        if (video.dataset.playRequested === 'true' && video.paused !== true) return;
        if (typeof video.play !== 'function') return;
        video.dataset.playRequested = 'true';
        const promise = video.play();
        if (promise && typeof promise.catch === 'function') {
            promise.catch(function() {
                video.dataset.playRequested = 'false';
                video.controls = true;
            });
        }
    }

    function initializeVideoFallback(video) {
        if (video.dataset.fallbackReady === 'true') return;
        video.dataset.fallbackReady = 'true';
        const fallback = video.parentElement && video.parentElement.querySelector('img[data-gallery-fallback]');
        if (fallback) {
            fallback.addEventListener('load', function() {
                if (video.dataset.fallbackLoading !== 'true') return;
                video.dataset.fallbackLoading = 'false';
                video.dataset.fallbackShown = 'true';
                fallback.hidden = false;
                video.hidden = true;
                pauseVideo(video);
            });
            fallback.addEventListener('error', function() {
                if (video.dataset.fallbackLoading !== 'true') return;
                video.dataset.fallbackLoading = 'false';
                video.dataset.fallbackFailed = 'true';
                fallback.hidden = true;
                video.hidden = false;
                video.controls = true;
            });
        }
        function noteUserGesture() {
            video.dataset.userGestureAt = String(Date.now());
            if (typeof window.setTimeout === 'function') {
                const gesture = video.dataset.userGestureAt;
                window.setTimeout(function() {
                    if (video.dataset.userGestureAt === gesture) delete video.dataset.userGestureAt;
                }, 1000);
            }
        }
        function usedRecentGesture() {
            const gestureAt = Number(video.dataset.userGestureAt || 0);
            delete video.dataset.userGestureAt;
            return gestureAt > 0 && Date.now() - gestureAt <= 1000;
        }
        video.addEventListener('pointerdown', noteUserGesture);
        video.addEventListener('keydown', noteUserGesture);
        video.addEventListener('play', function() {
            video.dataset.playRequested = 'true';
            if (usedRecentGesture()) video.dataset.userPaused = 'false';
        });
        video.addEventListener('pause', function() {
            video.dataset.playRequested = 'false';
            if (video.dataset.managedPause === 'true') {
                delete video.dataset.managedPause;
                delete video.dataset.userGestureAt;
            } else if (usedRecentGesture()) {
                video.dataset.userPaused = 'true';
            }
        });
        video.addEventListener('error', function() {
            if (video.dataset.fallbackShown === 'true' || video.dataset.fallbackLoading === 'true' || video.dataset.fallbackFailed === 'true') return;
            const fallbackUrl = video.dataset.gifFallback;
            if (!fallbackUrl || !fallback) {
                video.controls = true;
                return;
            }

            // Keep the poster/video in place while the GIF loads. Revealing the
            // fallback first exposes its alt text as a second media column on
            // slower connections, especially in iOS Safari.
            video.dataset.fallbackLoading = 'true';
            fallback.loading = 'eager';
            fallback.src = fallbackUrl;
        });
    }

    function observeVisibility(element, onChange) {
        if (typeof window.IntersectionObserver !== 'function') {
            onChange(true);
            return;
        }

        const observer = new window.IntersectionObserver(function(entries) {
            entries.forEach(function(entry) {
                if (entry.target === element) onChange(entry.isIntersecting);
            });
        }, { threshold: 0.01 });
        observer.observe(element);
    }

    function initializeStandaloneAnimation(container) {
        const video = container.querySelector('video[data-gallery-video]');
        if (!video) return;
        let visible = false;
        function updatePlayback() {
            if (visible && !document.hidden) {
                playVideo(video);
            } else {
                pauseVideo(video);
            }
        }
        initializeVideoFallback(video);
        observeVisibility(container, function(isVisible) {
            visible = isVisible;
            updatePlayback();
        });
        document.addEventListener('visibilitychange', updatePlayback);
    }

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
        let initialized = false;
        let galleryVisible = false;

        slides.forEach(function(slide) {
            const stage = slide.querySelector('.feed_gallery_stage') || slide;
            slide.querySelectorAll('video[data-gallery-video]').forEach(function(video) {
                initializeVideoFallback(video);
                video.dataset.mediaVisible = 'false';
                observeVisibility(stage, function(visible) {
                    video.dataset.mediaVisible = visible ? 'true' : 'false';
                    if (initialized) scheduleCurrentSlideUpdate();
                });
            });
        });

        function loadSlide(index) {
            const slide = slides[index];
            if (!slide) return;

            const loadedImage = slide.querySelector('img[src]');
            if (loadedImage) loadedImage.loading = 'eager';
            const image = slide.querySelector('img[data-src]');
            if (image) {
                image.loading = 'eager';
                image.src = image.dataset.src;
                image.removeAttribute('data-src');
            }

            const video = slide.querySelector('video[data-gallery-video]');
            if (video) {
                prepareVideo(video);
            }
        }

        function loadAdjacentSlides(index) {
            loadSlide(index - 1);
            loadSlide(index);
            loadSlide(index + 1);
        }

        function useSlideAspectRatio(slide) {
            const width = Number(slide && slide.dataset.galleryWidth);
            const height = Number(slide && slide.dataset.galleryHeight);
            if (width > 0 && height > 0) {
                gallery.style.setProperty('--gallery-active-aspect-ratio', `${width} / ${height}`);
            }
        }

        function updateCurrentSlide() {
            scrollFrame = 0;
            if (!initialized) return;
            let closestIndex = 0;
            let closestDistance = Infinity;

            slides.forEach(function(slide, index) {
                const distance = Math.abs(slide.offsetLeft - track.scrollLeft);
                if (distance < closestDistance) {
                    closestDistance = distance;
                    closestIndex = index;
                }
            });

            useSlideAspectRatio(slides[closestIndex]);
            current.textContent = String(closestIndex + 1);
            loadAdjacentSlides(closestIndex);
            slides.forEach(function(slide, index) {
                slide.querySelectorAll('video[data-gallery-video]').forEach(function(video) {
                    if (index === closestIndex && galleryVisible && video.dataset.mediaVisible === 'true' && !document.hidden) {
                        playVideo(video);
                    } else {
                        pauseVideo(video);
                    }
                });
            });
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
        window.addEventListener('pageshow', scheduleCurrentSlideUpdate);
        document.addEventListener('visibilitychange', function() {
            if (document.hidden) {
                slides.forEach(function(slide) {
                    slide.querySelectorAll('video[data-gallery-video]').forEach(pauseVideo);
                });
            } else {
                scheduleCurrentSlideUpdate();
            }
        });

        function showSlide(index) {
            const slide = slides[index];
            if (!slide) return;
            useSlideAspectRatio(slide);
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

        observeVisibility(gallery, function(visible) {
            galleryVisible = visible;
            if (visible) {
                initialized = true;
                updateCurrentSlide();
            } else if (initialized) {
                slides.forEach(function(slide) {
                    slide.querySelectorAll('video[data-gallery-video]').forEach(pauseVideo);
                });
            }
        });
    }

    function initializeGalleries() {
        document.body.classList.add('gallery-js');
        document.querySelectorAll('[data-gallery-standalone]').forEach(initializeStandaloneAnimation);
        document.querySelectorAll('[data-gallery]').forEach(initializeGallery);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', initializeGalleries);
    } else {
        initializeGalleries();
    }
})();
