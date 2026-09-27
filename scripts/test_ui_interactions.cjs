// Dependency-free behavior tests for the real browser scripts. These DOM
// doubles exercise event/state logic, not browser layout or native scrolling.
// Run with: node --test scripts/test_ui_interactions.cjs
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

class Events {
    constructor() {
        this.listeners = new Map();
    }

    addEventListener(type, listener) {
        if (!this.listeners.has(type)) this.listeners.set(type, []);
        this.listeners.get(type).push(listener);
    }

    emit(type, properties = {}) {
        const event = {
            defaultPrevented: false,
            propagationStopped: false,
            preventDefault() { this.defaultPrevented = true; },
            stopPropagation() { this.propagationStopped = true; },
            ...properties,
        };
        for (const listener of this.listeners.get(type) || []) listener(event);
        return event;
    }
}

class Element extends Events {
    constructor(tag = 'div', attributes = {}) {
        super();
        this.tag = tag;
        this.attributes = { ...attributes };
        this.dataset = {};
        this.children = [];
        this.parentElement = null;
        this.textContent = '';
        const styles = new Map();
        this.style = {
            setProperty: (name, value) => styles.set(name, value),
            getPropertyValue: name => styles.get(name) || '',
        };
        this.loading = attributes.loading || '';
        this._src = attributes.src || '';
        this.srcAssignments = [];
        Object.defineProperty(this, 'src', {
            get: () => this._src,
            set: value => {
                this.srcAssignments.push({ value, loading: this.loading });
                this._src = value;
            },
        });
        this.hidden = 'hidden' in attributes;
        const classes = new Set((attributes.class || '').split(/\s+/).filter(Boolean));
        this.classList = {
            add: (...names) => names.forEach(name => classes.add(name)),
            remove: (...names) => names.forEach(name => classes.delete(name)),
            contains: name => classes.has(name),
        };
        this.clientWidth = 0;
        this.getBoundingClientRect = () => ({ width: this.clientWidth, height: 0 });
        for (const [name, value] of Object.entries(attributes)) {
            if (name.startsWith('data-')) this.dataset[this.dataKey(name)] = value;
        }
    }

    dataKey(name) {
        return name.slice(5).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
    }

    removeAttribute(name) {
        delete this.attributes[name];
        if (name.startsWith('data-')) delete this.dataset[this.dataKey(name)];
    }

    append(...children) {
        for (const child of children) {
            child.parentElement = this;
            this.children.push(child);
        }
        return this;
    }

    matches(selector) {
        if (selector.startsWith('.')) return this.classList.contains(selector.slice(1));
        const match = selector.match(/^(\w+)?(?:\[([^\]]+)\])?$/);
        assert.ok(match, `Unsupported test selector: ${selector}`);
        return (!match[1] || match[1] === this.tag) && (!match[2] || match[2] in this.attributes);
    }

    querySelectorAll(selector) {
        return this.children.flatMap(child => [
            ...(child.matches(selector) ? [child] : []),
            ...child.querySelectorAll(selector),
        ]);
    }

    querySelector(selector) {
        return this.querySelectorAll(selector)[0] || null;
    }

    closest(selector) {
        if (this.matches(selector)) return this;
        return this.parentElement ? this.parentElement.closest(selector) : null;
    }
}

function browser({ coarse = false, readyState = 'complete', intersection = false } = {}) {
    let now = 10_000;
    let frameId = 0;
    let timerId = 0;
    const frames = new Map();
    const timers = new Map();
    const window = new Events();
    const document = new Events();
    document.body = new Element('body');
    document.readyState = readyState;
    document.querySelectorAll = selector => document.body.querySelectorAll(selector);
    window.matchMedia = () => ({ matches: coarse });
    window.innerHeight = 1000;
    const observers = [];
    if (intersection) {
        window.IntersectionObserver = class {
            constructor(callback) {
                this.callback = callback;
                observers.push(this);
            }
            observe(target) { this.target = target; }
        };
    }
    window.requestAnimationFrame = callback => {
        frames.set(++frameId, callback);
        return frameId;
    };
    window.setTimeout = callback => {
        timers.set(++timerId, callback);
        return timerId;
    };
    window.clearTimeout = id => timers.delete(id);
    const context = vm.createContext({ window, document, Element, Date: { now: () => now } });
    return {
        window,
        document,
        advance: milliseconds => { now += milliseconds; },
        intersect(target, isIntersecting) {
            observers.filter(observer => observer.target === target).forEach(observer => {
                observer.callback([{ target, isIntersecting }]);
            });
        },
        run(name) {
            const filename = path.join(__dirname, '..', 'static', name);
            vm.runInContext(fs.readFileSync(filename, 'utf8'), context, { filename });
        },
        flushFrames() {
            const pending = [...frames.values()];
            frames.clear();
            pending.forEach(callback => callback());
            return pending.length;
        },
        flushTimers() {
            const pending = [...timers.values()];
            timers.clear();
            pending.forEach(callback => callback());
            return pending.length;
        },
    };
}

function galleryFixture(options) {
    const env = browser(options);
    const gallery = new Element('div', { 'data-gallery': '', 'data-gallery-mode': options?.mode || 'feed' });
    const viewport = new Element('div', { 'data-gallery-viewport': '' });
    const track = new Element('div', { 'data-gallery-track': '' });
    const current = new Element('span', { 'data-gallery-current': '' });
    const previous = new Element('button', { 'data-gallery-previous': '' });
    const next = new Element('button', { 'data-gallery-next': '' });
    const images = [];
    const captions = [];
    const originals = [];
    const slides = Array.from({ length: 5 }, (_, index) => {
        const slide = new Element('figure', {
            'data-gallery-slide': '',
            'data-gallery-width': String(index === 1 ? 900 : 1600),
            'data-gallery-height': String(index === 1 ? 1600 : 900),
        });
        const image = new Element('img', { [index === 0 ? 'src' : 'data-src']: `/preview/${index}.jpg` });
        images.push(image);
        slide.offsetLeft = index * 300;
        return slide.append(image);
    });
    for (let index = 0; index < slides.length; index += 1) {
        captions.push(new Element('div', { 'data-gallery-caption': '', ...(index === 0 ? {} : { hidden: '' }) }));
        originals.push(new Element('a', { 'data-gallery-original-control': '', ...(index === 0 ? {} : { hidden: '' }) }));
    }
    gallery.clientWidth = options?.viewportWidth || 300;
    viewport.clientWidth = options?.viewportWidth || 300;
    viewport.getBoundingClientRect = () => ({
        width: viewport.clientWidth,
        height: Number.parseFloat(gallery.style.getPropertyValue('--gallery-media-height')) || 0,
    });
    track.scrollLeft = 0;
    track.scrollTo = options => {
        track.lastScroll = { ...options };
        track.scrollLeft = options.left;
        track.emit('scroll');
    };
    track.append(...slides);
    viewport.append(track, previous, next);
    gallery.append(viewport, ...captions, ...originals, current);
    env.document.body.append(gallery);
    env.run('gallery.js');
    return { ...env, gallery, viewport, track, current, previous, next, images, slides, captions, originals };
}

const pointer = (properties = {}) => ({
    isPrimary: true, pointerId: 1, pointerType: 'touch', clientX: 10, clientY: 10, ...properties,
});

test('gallery initializes only the first and adjacent previews', () => {
    const env = galleryFixture();
    assert.equal(env.document.body.classList.contains('gallery-js'), true);
    assert.deepEqual(env.images.map(image => image.src), ['/preview/0.jpg', '/preview/1.jpg', '', '', '']);
    assert.equal(env.current.textContent, '1');
    assert.equal(env.previous.disabled, true);
    assert.equal(env.next.disabled, false);
    assert.equal(env.gallery.style.getPropertyValue('--gallery-media-height'), '169px');
    assert.equal('src' in env.images[1].dataset, false);
    assert.deepEqual(env.images[1].srcAssignments, [{ value: '/preview/1.jpg', loading: 'eager' }]);
});

test('gallery defers JavaScript-managed loading until it approaches the viewport', () => {
    const env = galleryFixture({ intersection: true });
    assert.equal(env.images[1].src, '');
    env.intersect(env.gallery, true);
    assert.equal(env.images[1].src, '/preview/1.jpg');
    assert.equal(env.current.textContent, '1');
});

test('gallery animation uses MP4, pauses off-slide, and falls back to GIF once', () => {
    const env = browser();
    const gallery = new Element('div', { 'data-gallery': '', 'data-gallery-mode': 'feed' });
    const viewport = new Element('div', { 'data-gallery-viewport': '' });
    viewport.clientWidth = 300;
    const track = new Element('div', { 'data-gallery-track': '' });
    const current = new Element('span', { 'data-gallery-current': '' });
    const previous = new Element('button', { 'data-gallery-previous': '' });
    const next = new Element('button', { 'data-gallery-next': '' });
    const animated = new Element('figure', { 'data-gallery-slide': '' });
    const stage = new Element('div', { class: 'feed_gallery_stage' });
    const video = new Element('video', {
        'data-gallery-video': '',
        'data-src': '/preview/animation.mp4',
        'data-poster': '/preview/poster.jpg',
        'data-gif-fallback': '/img/animation.gif',
        'data-autoplay': 'true',
    });
    const fallback = new Element('img', { 'data-gallery-fallback': '', hidden: '' });
    video.loadCount = 0;
    video.playCount = 0;
    video.pauseCount = 0;
    video.paused = true;
    video.load = () => { video.loadCount += 1; };
    video.play = () => {
        video.playCount += 1;
        video.paused = false;
        video.emit('play');
        return Promise.resolve();
    };
    video.pause = () => {
        video.pauseCount += 1;
        video.paused = true;
        video.emit('pause');
    };
    animated.offsetLeft = 0;
    animated.append(stage.append(video, fallback));
    const still = new Element('figure', { 'data-gallery-slide': '' });
    still.offsetLeft = 300;
    still.append(new Element('img', { 'data-src': '/preview/still.jpg' }));
    track.scrollLeft = 0;
    track.scrollTo = options => {
        track.scrollLeft = options.left;
        track.emit('scroll');
    };
    track.append(animated, still);
    viewport.append(track, previous, next);
    gallery.append(viewport, current);
    env.document.body.append(gallery);
    env.run('gallery.js');

    assert.equal(video.poster, '/preview/poster.jpg');
    assert.equal(video.src, '/preview/animation.mp4');
    assert.equal(video.loadCount, 1);
    assert.equal(video.playCount, 1);
    next.emit('click');
    env.flushFrames();
    env.flushTimers();
    assert.ok(video.pauseCount > 0);

    previous.emit('click');
    env.flushFrames();
    env.flushTimers();
    assert.equal(video.playCount, 2);

    video.emit('pointerdown');
    next.emit('click');
    env.flushFrames();
    env.flushTimers();
    previous.emit('click');
    env.flushFrames();
    env.flushTimers();
    assert.equal(video.playCount, 3, 'swiping away does not count as a manual pause');

    video.emit('pointerdown');
    video.paused = true;
    video.emit('pause');
    env.window.emit('resize');
    env.flushFrames();
    assert.equal(video.playCount, 3, 'a user-paused animation stays paused');

    video.emit('error');
    assert.equal(video.hidden, false, 'video remains in place while the GIF fallback loads');
    assert.equal(fallback.hidden, true, 'fallback alt text is not exposed while loading');
    assert.deepEqual(fallback.srcAssignments, [{ value: '/img/animation.gif', loading: 'eager' }]);
    video.emit('error');
    assert.equal(fallback.srcAssignments.length, 1, 'fallback is activated only once');
    fallback.emit('load');
    assert.equal(video.hidden, true);
    assert.equal(fallback.hidden, false);
    assert.ok(video.pauseCount > 0);
});

test('gallery keeps the video visible if its GIF fallback also fails', () => {
    const env = browser();
    const container = new Element('div', { 'data-gallery-standalone': '' });
    const video = new Element('video', {
        'data-gallery-video': '',
        'data-src': '/preview/animation.mp4',
        'data-gif-fallback': '/img/animation.gif',
        'data-autoplay': 'false',
    });
    const fallback = new Element('img', { 'data-gallery-fallback': '', hidden: '' });
    video.paused = true;
    video.load = () => {};
    video.pause = () => {};
    container.append(video, fallback);
    env.document.body.append(container);
    env.run('gallery.js');

    video.emit('error');
    fallback.emit('error');
    assert.equal(video.hidden, false);
    assert.equal(video.controls, true);
    assert.equal(fallback.hidden, true);
    video.emit('error');
    assert.equal(fallback.srcAssignments.length, 1, 'a failed fallback is not retried in a loop');
});

test('gallery stages use each item aspect ratio without a fixed black frame', () => {
    const template = fs.readFileSync(path.join(__dirname, '..', 'templates', 'utils.html'), 'utf8');
    const stylesheet = fs.readFileSync(path.join(__dirname, '..', 'static', 'style.css'), 'utf8');

    assert.ok(template.includes('style="--gallery-aspect-ratio: {{ image.width }} / {{ image.height }}"'));
    assert.ok(template.includes('data-gallery-width="{{ image.width }}" data-gallery-height="{{ image.height }}"'));
    assert.match(stylesheet, /\.feed_gallery_stage \{[^}]+aspect-ratio: var\(--gallery-aspect-ratio, 16 \/ 9\);[^}]+background: transparent;/s);
    assert.ok(template.includes('{% call render_gallery_carousel(post.gallery, "detail") %}'));
    assert.ok(template.includes('{% call render_gallery_carousel(post.gallery, "feed") %}'));
    assert.ok(template.includes('data-gallery-viewport'));
    assert.equal((template.match(/\{% for image in &images/g) || []).length, 3, 'shared gallery media is always borrowed');
    assert.doesNotMatch(template, /\{% for image in images/);
    assert.ok(template.includes('class="gallery gallery_detail"'), 'single-item detail galleries keep responsive gallery styles');
    assert.match(stylesheet, /\.gallery-js \.adaptive_gallery \.feed_gallery_viewport \{[^}]+height: var\(--gallery-media-height, auto\);/s);
    assert.match(stylesheet, /\.gallery-js \.feed_gallery_control \{[^}]+width: 44px;[^}]+height: 44px;/s);
    assert.match(stylesheet, /\.gallery_detail > figure > a:not\(\.gallery_original_link\) > img,[^{]+\{[^}]+height: auto;/s);
    assert.doesNotMatch(stylesheet, /--gallery-active-aspect-ratio/);
    assert.doesNotMatch(stylesheet, /\.feed_gallery_stage \{[^}]+height: clamp\(/s);
    assert.match(stylesheet, /\.gallery_detail_animation > video,[^}]+grid-area: 1 \/ 1;/s);
});

test('gallery animation loads without playing when autoplay is disabled', () => {
    const env = browser();
    const container = new Element('div', { 'data-gallery-standalone': '' });
    const video = new Element('video', {
        'data-gallery-video': '',
        'data-src': '/preview/animation.mp4',
        'data-autoplay': 'false',
    });
    video.paused = true;
    video.loadCount = 0;
    video.playCount = 0;
    video.load = () => { video.loadCount += 1; };
    video.play = () => { video.playCount += 1; return Promise.resolve(); };
    video.pause = () => {};
    container.append(video);
    env.document.body.append(container);
    env.run('gallery.js');

    assert.equal(video.src, '/preview/animation.mp4');
    assert.equal(video.loadCount, 1);
    assert.equal(video.playCount, 0);
});

test('gallery animation playback follows the media stage visibility', () => {
    const env = browser({ intersection: true });
    const gallery = new Element('div', { 'data-gallery': '', 'data-gallery-mode': 'feed' });
    const viewport = new Element('div', { 'data-gallery-viewport': '' });
    viewport.clientWidth = 300;
    const track = new Element('div', { 'data-gallery-track': '' });
    const current = new Element('span', { 'data-gallery-current': '' });
    const previous = new Element('button', { 'data-gallery-previous': '' });
    const next = new Element('button', { 'data-gallery-next': '' });
    const animated = new Element('figure', { 'data-gallery-slide': '' });
    const stage = new Element('div', { class: 'feed_gallery_stage' });
    const video = new Element('video', {
        'data-gallery-video': '',
        'data-src': '/preview/animation.mp4',
        'data-autoplay': 'true',
    });
    video.paused = true;
    video.playCount = 0;
    video.pauseCount = 0;
    video.load = () => {};
    video.play = () => {
        video.playCount += 1;
        video.paused = false;
        video.emit('play');
        return Promise.resolve();
    };
    video.pause = () => {
        video.pauseCount += 1;
        video.paused = true;
        video.emit('pause');
    };
    animated.offsetLeft = 0;
    animated.append(stage.append(video));
    const still = new Element('figure', { 'data-gallery-slide': '' });
    still.offsetLeft = 300;
    still.append(new Element('img', { 'data-src': '/preview/still.jpg' }));
    track.scrollLeft = 0;
    track.append(animated, still);
    viewport.append(track, previous, next);
    gallery.append(viewport, current);
    env.document.body.append(gallery);
    env.run('gallery.js');

    env.intersect(gallery, true);
    assert.equal(video.playCount, 0, 'gallery visibility alone does not start playback');
    env.intersect(stage, true);
    env.flushFrames();
    assert.equal(video.playCount, 1);
    env.intersect(stage, false);
    env.flushFrames();
    assert.ok(video.pauseCount > 0);
});

test('gallery navigation updates loading, counter and boundary buttons', () => {
    const env = galleryFixture();
    env.next.emit('click');
    assert.equal(env.gallery.style.getPropertyValue('--gallery-media-height'), '169px', 'height is frozen during navigation');
    assert.deepEqual(env.track.lastScroll, { left: 300, behavior: 'smooth' });
    assert.equal(env.images[2].src, '/preview/2.jpg');
    assert.equal(env.images[3].src, '');
    env.flushFrames();
    assert.equal(env.current.textContent, '2');
    assert.equal(env.previous.disabled, false);
    assert.equal(env.gallery.style.getPropertyValue('--gallery-media-height'), '169px');
    assert.equal(env.captions[0].hidden, false, 'caption remains stable until the slide settles');
    assert.equal(env.captions[1].hidden, true);
    env.flushTimers();
    assert.equal(env.gallery.style.getPropertyValue('--gallery-media-height'), '533px');
    assert.equal(env.captions[0].hidden, true);
    assert.equal(env.captions[1].hidden, false);
    assert.equal(env.originals[0].hidden, true);
    assert.equal(env.originals[1].hidden, false);

    env.track.scrollLeft = 1190;
    env.track.emit('scroll');
    env.track.emit('scroll');
    assert.equal(env.flushFrames(), 1, 'scroll updates are coalesced into one frame');
    assert.equal(env.current.textContent, '5');
    assert.equal(env.next.disabled, true);
    assert.equal(env.images[3].src, '/preview/3.jpg');
    assert.equal(env.images[4].src, '/preview/4.jpg');
    env.flushTimers();
    assert.equal(env.gallery.style.getPropertyValue('--gallery-media-height'), '169px', 'landscape slide sheds the portrait height');

    env.previous.emit('click');
    env.flushFrames();
    env.flushTimers();
    assert.equal(env.current.textContent, '4');
    assert.equal(env.next.disabled, false);

    env.viewport.clientWidth = 400;
    env.window.emit('resize');
    assert.equal(env.gallery.style.getPropertyValue('--gallery-media-height'), '225px');
});

test('late media dimensions cannot resize a gallery while touch scrolling settles', () => {
    const env = galleryFixture();
    env.track.emit('pointerdown', pointer());
    env.window.emit('pointercancel', pointer());
    env.images[0].naturalWidth = 100;
    env.images[0].naturalHeight = 1000;
    env.images[0].emit('load');
    assert.equal(env.gallery.style.getPropertyValue('--gallery-media-height'), '169px');
    assert.equal(env.gallery.classList.contains('gallery_dragging'), true);

    env.flushTimers();
    assert.equal(env.gallery.style.getPropertyValue('--gallery-media-height'), '560px');
    assert.equal(env.gallery.classList.contains('gallery_dragging'), false);
});

test('detail galleries use the larger viewport cap without oversized portrait slides', () => {
    const feed = galleryFixture({ viewportWidth: 1000 });
    feed.next.emit('click');
    feed.flushFrames();
    feed.flushTimers();
    assert.equal(feed.gallery.style.getPropertyValue('--gallery-media-height'), '560px');

    const detail = galleryFixture({ mode: 'detail', viewportWidth: 1000 });
    detail.next.emit('click');
    detail.flushFrames();
    detail.flushTimers();
    assert.equal(detail.gallery.style.getPropertyValue('--gallery-media-height'), '720px');
});

test('gallery waits for DOMContentLoaded when needed', () => {
    const env = galleryFixture({ readyState: 'loading' });
    assert.equal(env.images[1].src, '');
    env.document.emit('DOMContentLoaded');
    assert.equal(env.current.textContent, '1');
    assert.equal(env.images[1].src, '/preview/1.jpg');
});

test('gallery suppresses drag navigation even after a long hold before release', () => {
    const env = galleryFixture();
    env.track.emit('pointerdown', pointer());
    env.window.emit('pointermove', pointer({ clientX: 30 }));
    env.advance(2000);
    env.window.emit('pointerup', pointer());
    const click = env.track.emit('click', { detail: 1 });
    assert.equal(click.defaultPrevented, true);
    assert.equal(click.propagationStopped, true);
    assert.equal(env.track.emit('click', { detail: 1 }).defaultPrevented, false, 'suppression is consumed once');
});

test('gallery preserves taps and keyboard activation and ignores unrelated pointers', () => {
    const env = galleryFixture();
    env.track.emit('pointerdown', pointer());
    env.window.emit('pointermove', pointer({ pointerId: 2, clientX: 100 }));
    env.window.emit('pointermove', pointer({ clientX: 14 }));
    env.window.emit('pointerup', pointer());
    assert.equal(env.track.emit('click', { detail: 1 }).defaultPrevented, false);

    env.track.emit('pointerdown', pointer());
    env.window.emit('pointermove', pointer({ clientX: 40 }));
    env.window.emit('pointerup', pointer());
    assert.equal(env.track.emit('click', { detail: 0 }).defaultPrevented, false);
    env.advance(501);
    assert.equal(env.track.emit('click', { detail: 1 }).defaultPrevented, false);

    env.track.emit('pointerdown', pointer());
    env.window.emit('pointermove', pointer({ clientX: 40 }));
    env.window.emit('pointercancel', pointer());
    assert.equal(env.track.emit('click', { detail: 1 }).defaultPrevented, false);
});

function spoilerFixture(options) {
    const env = browser(options);
    env.run('spoilers.js');
    const spoiler = new Element('span', { class: 'md-spoiler-text' });
    const link = new Element('a');
    const emphasis = new Element('strong');
    spoiler.append(link.append(emphasis));
    env.document.body.append(spoiler);
    env.tap = (target, pointerType = 'touch') => {
        env.document.emit('pointerdown', pointer({ pointerType }));
        return env.document.emit('click', { target, detail: 1, pointerType });
    };
    return { ...env, spoiler, link, emphasis };
}

test('first tap anywhere in a spoiler reveals it; second link tap navigates', () => {
    const env = spoilerFixture();
    const first = env.tap(env.emphasis);
    assert.equal(first.defaultPrevented, true);
    assert.equal(first.propagationStopped, true);
    assert.equal(env.spoiler.classList.contains('spoiler_revealed'), true);
    assert.equal(env.tap(env.link).defaultPrevented, false);

    const text = spoilerFixture();
    assert.equal(text.tap(text.spoiler).defaultPrevented, true);
    assert.equal(text.spoiler.classList.contains('spoiler_revealed'), true);
});

test('tapping a nested spoiler reveals the whole enclosing spoiler', () => {
    const env = spoilerFixture();
    const nested = new Element('span', { class: 'md-spoiler-text' });
    const sibling = new Element('span', { class: 'md-spoiler-text' });
    env.spoiler.append(nested, sibling);
    assert.equal(env.tap(nested).defaultPrevented, true);
    for (const element of [env.spoiler, nested, sibling]) {
        assert.equal(element.classList.contains('spoiler_revealed'), true);
    }
    assert.equal(env.tap(env.link).defaultPrevented, false);
});

test('spoilers preserve keyboard and explicit mouse behavior on hybrid devices', () => {
    const env = spoilerFixture({ coarse: true });
    env.document.emit('pointerdown', pointer());
    assert.equal(env.document.emit('click', { target: env.link, detail: 0, pointerType: 'touch' }).defaultPrevented, false);
    assert.equal(env.spoiler.classList.contains('spoiler_revealed'), false);
    assert.equal(env.tap(env.link, 'mouse').defaultPrevented, false);
    assert.equal(env.spoiler.classList.contains('spoiler_revealed'), true);
    assert.equal(env.tap(env.link, 'touch').defaultPrevented, false);

    const desktop = spoilerFixture({ coarse: false });
    assert.equal(desktop.document.emit('click', { target: desktop.link, detail: 1 }).defaultPrevented, false);
    assert.equal(desktop.spoiler.classList.contains('spoiler_revealed'), true);
    const pen = spoilerFixture();
    assert.equal(pen.tap(pen.link, 'pen').defaultPrevented, true);
});

test('revealing a second spoiler keeps both revealed for touch, pen and mouse clicks', () => {
    for (const pointerType of ['touch', 'pen', 'mouse', '']) {
        const env = spoilerFixture({ coarse: false });
        const second = new Element('span', { class: 'md-spoiler-text' });
        env.document.body.append(second);
        const activate = target => pointerType
            ? env.tap(target, pointerType)
            : env.document.emit('click', { target, detail: 1 });
        activate(env.spoiler);
        assert.equal(env.spoiler.classList.contains('spoiler_revealed'), true, pointerType);
        activate(second);
        assert.equal(env.spoiler.classList.contains('spoiler_revealed'), true, pointerType);
        assert.equal(second.classList.contains('spoiler_revealed'), true, pointerType);
        activate(env.spoiler);
        assert.equal(env.spoiler.classList.contains('spoiler_revealed'), true, pointerType);
        assert.equal(second.classList.contains('spoiler_revealed'), true, pointerType);
    }
});

test('keyboard and non-primary mouse activation do not reveal spoilers', () => {
    const env = spoilerFixture();
    for (const properties of [{ detail: 0 }, { detail: 1, button: 1 }, { detail: 1, button: 2 }]) {
        const event = env.document.emit('click', { target: env.link, pointerType: 'mouse', ...properties });
        assert.equal(event.defaultPrevented, false);
        assert.equal(env.spoiler.classList.contains('spoiler_revealed'), false);
    }
});

test('spoiler fallback supports touch-only browsers and recent touch on hybrids', () => {
    const env = spoilerFixture({ coarse: false });
    env.document.emit('touchstart', { touches: [{ clientX: 10, clientY: 10 }] });
    assert.equal(env.document.emit('click', { target: env.link, detail: 1 }).defaultPrevented, true);

    const coarse = spoilerFixture({ coarse: true });
    assert.equal(coarse.document.emit('click', { target: coarse.link, detail: 1 }).defaultPrevented, true);

    const expired = spoilerFixture({ coarse: false });
    expired.document.emit('touchstart', { touches: [{ clientX: 10, clientY: 10 }] });
    expired.advance(1001);
    assert.equal(expired.document.emit('click', { target: expired.link, detail: 1 }).defaultPrevented, false);
});

test('scrolling over a spoiler does not reveal it and the next tap still works', () => {
    for (const touchOnly of [false, true]) {
        const env = spoilerFixture({ coarse: true });
        if (touchOnly) {
            env.document.emit('touchstart', { touches: [{ clientX: 10, clientY: 10 }] });
            env.document.emit('touchmove', { touches: [{ clientX: 10, clientY: 30 }] });
        } else {
            env.document.emit('pointerdown', pointer());
            env.document.emit('pointermove', pointer({ clientY: 30 }));
        }
        env.document.emit('click', { target: env.link, detail: 1, ...(touchOnly ? {} : { pointerType: 'touch' }) });
        assert.equal(env.spoiler.classList.contains('spoiler_revealed'), false);
        assert.equal(env.tap(env.link).defaultPrevented, true);
    }
});

test('unrelated and non-element click targets are left alone', () => {
    const env = spoilerFixture({ coarse: true });
    assert.equal(env.tap(new Element('a')).defaultPrevented, false);
    assert.equal(env.tap({}).defaultPrevented, false);
});
