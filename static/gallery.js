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
        const viewport = gallery.querySelector('[data-gallery-viewport]');
        const slides = Array.from(gallery.querySelectorAll('[data-gallery-slide]'));
        const captions = Array.from(gallery.querySelectorAll('[data-gallery-caption]'));
        const originalControls = Array.from(gallery.querySelectorAll('[data-gallery-original-control]'));
        const current = gallery.querySelector('[data-gallery-current]');
        const previous = gallery.querySelector('[data-gallery-previous]');
        const next = gallery.querySelector('[data-gallery-next]');
        if (!track || !viewport || slides.length < 2 || !current) return;

        let scrollFrame = 0;
        let settleTimer = 0;
        let pointerStart = null;
        let activePointerId = null;
        let pointerMoved = false;
        let suppressClickUntil = 0;
        let initialized = false;
        let galleryVisible = false;
        let activeIndex = 0;
        let settledIndex = 0;

        function viewportWidth() {
            return viewport.clientWidth || gallery.clientWidth || track.clientWidth || slides[0].clientWidth || 1;
        }

        function viewportHeightLimit() {
            const visualHeight = Number(window.visualViewport && window.visualViewport.height);
            const windowHeight = Number(window.innerHeight);
            const availableHeight = visualHeight > 0 ? visualHeight : (windowHeight > 0 ? windowHeight : 800);
            const detail = gallery.dataset.galleryMode === 'detail';
            return Math.min(availableHeight * (detail ? 0.76 : 0.68), detail ? 720 : 560);
        }

        function dimensionsForSlide(slide) {
            const width = Number(slide && slide.dataset.galleryWidth);
            const height = Number(slide && slide.dataset.galleryHeight);
            return width > 0 && height > 0 ? { width, height } : { width: 4, height: 3 };
        }

        function mediaHeightForSlide(slide) {
            const dimensions = dimensionsForSlide(slide);
            const naturalHeight = viewportWidth() * dimensions.height / dimensions.width;
            return Math.max(1, Math.round(Math.min(naturalHeight, viewportHeightLimit())));
        }

        function resizeToLargestSlide() {
            const height = Math.max(...slides.map(mediaHeightForSlide));
            gallery.style.setProperty('--gallery-media-height', `${height}px`);
            gallery.style.setProperty('--gallery-control-top', `${Math.round(height / 2)}px`);
        }

        function canResizeGallery() {
            return initialized && activePointerId === null && settleTimer === 0 && !gallery.classList.contains('gallery_dragging');
        }

        function freezeViewportHeight() {
            if (typeof viewport.getBoundingClientRect !== 'function') return;
            const height = viewport.getBoundingClientRect().height;
            if (height > 0) gallery.style.setProperty('--gallery-media-height', `${Math.round(height)}px`);
        }

        function closestSlideIndex() {
            let closestIndex = 0;
            let closestDistance = Infinity;
            slides.forEach(function(slide, index) {
                const distance = Math.abs(slide.offsetLeft - track.scrollLeft);
                if (distance < closestDistance) {
                    closestDistance = distance;
                    closestIndex = index;
                }
            });
            return closestIndex;
        }

        function showActiveMetadata(index) {
            captions.forEach(function(caption, captionIndex) {
                caption.hidden = captionIndex !== index;
            });
            originalControls.forEach(function(link, linkIndex) {
                link.hidden = linkIndex !== index;
            });
        }

        slides.forEach(function(slide) {
            const stage = slide.querySelector('.feed_gallery_stage') || slide;
            slide.querySelectorAll('img').forEach(function(image) {
                image.addEventListener('load', function() {
                    if (image.naturalWidth > 0 && image.naturalHeight > 0) {
                        slide.dataset.galleryWidth = String(image.naturalWidth);
                        slide.dataset.galleryHeight = String(image.naturalHeight);
                        if (canResizeGallery()) resizeToLargestSlide();
                    }
                });
            });
            slide.querySelectorAll('video[data-gallery-video]').forEach(function(video) {
                initializeVideoFallback(video);
                video.dataset.mediaVisible = 'false';
                video.addEventListener('loadedmetadata', function() {
                    if (video.videoWidth > 0 && video.videoHeight > 0) {
                        slide.dataset.galleryWidth = String(video.videoWidth);
                        slide.dataset.galleryHeight = String(video.videoHeight);
                        if (canResizeGallery()) resizeToLargestSlide();
                    }
                });
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

        function updatePlayback() {
            slides.forEach(function(slide, slideIndex) {
                slide.querySelectorAll('video[data-gallery-video]').forEach(function(video) {
                    if (slideIndex === settledIndex && galleryVisible && video.dataset.mediaVisible === 'true' && !document.hidden) {
                        playVideo(video);
                    } else {
                        pauseVideo(video);
                    }
                });
            });
        }

        function updateCurrentSlide() {
            scrollFrame = 0;
            if (!initialized) return;
            activeIndex = closestSlideIndex();
            current.textContent = String(activeIndex + 1);
            loadAdjacentSlides(activeIndex);
            updatePlayback();
            if (previous) previous.disabled = activeIndex === 0;
            if (next) next.disabled = activeIndex === slides.length - 1;
        }

        function scheduleCurrentSlideUpdate() {
            if (scrollFrame === 0) {
                scrollFrame = window.requestAnimationFrame(updateCurrentSlide);
            }
        }

        function settleCurrentSlide() {
            if (!initialized || activePointerId !== null) return;
            if (settleTimer) {
                window.clearTimeout(settleTimer);
                settleTimer = 0;
            }
            updateCurrentSlide();
            settledIndex = activeIndex;
            gallery.classList.remove('gallery_dragging');
            resizeToLargestSlide();
            showActiveMetadata(settledIndex);
            updatePlayback();
        }

        function scheduleSettle() {
            if (settleTimer) window.clearTimeout(settleTimer);
            settleTimer = window.setTimeout(function() {
                settleTimer = 0;
                settleCurrentSlide();
            }, 140);
        }

        track.addEventListener('scroll', function() {
            scheduleCurrentSlideUpdate();
            scheduleSettle();
        }, { passive: true });
        track.addEventListener('scrollend', settleCurrentSlide);
        window.addEventListener('pageshow', function() {
            scheduleCurrentSlideUpdate();
            settleCurrentSlide();
        });
        window.addEventListener('orientationchange', scheduleSettle, { passive: true });
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
            loadAdjacentSlides(index);
            track.scrollTo({ left: slide.offsetLeft, behavior: 'smooth' });
            scheduleSettle();
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
            if (settleTimer) {
                window.clearTimeout(settleTimer);
                settleTimer = 0;
            }
            freezeViewportHeight();
            gallery.classList.add('gallery_dragging');
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
            scheduleSettle();
        });

        window.addEventListener('pointercancel', function(event) {
            if (event.pointerId !== activePointerId) return;
            pointerStart = null;
            activePointerId = null;
            pointerMoved = false;
            suppressClickUntil = 0;
            scheduleSettle();
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
                settledIndex = activeIndex;
                resizeToLargestSlide();
                showActiveMetadata(settledIndex);
                updatePlayback();
            } else if (initialized) {
                slides.forEach(function(slide) {
                    slide.querySelectorAll('video[data-gallery-video]').forEach(pauseVideo);
                });
            }
        });

        if (typeof window.ResizeObserver === 'function') {
            let observedWidth = gallery.clientWidth;
            const resizeObserver = new window.ResizeObserver(function(entries) {
                const width = entries[0] && entries[0].contentRect.width;
                if (!width || Math.abs(width - observedWidth) < 1) return;
                observedWidth = width;
                if (canResizeGallery()) resizeToLargestSlide();
            });
            resizeObserver.observe(gallery);
        } else {
            window.addEventListener('resize', function() {
                if (canResizeGallery()) resizeToLargestSlide();
            }, { passive: true });
        }
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
