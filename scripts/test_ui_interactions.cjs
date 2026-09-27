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
        this.src = attributes.src || '';
        const classes = new Set((attributes.class || '').split(/\s+/).filter(Boolean));
        this.classList = {
            add: (...names) => names.forEach(name => classes.add(name)),
            contains: name => classes.has(name),
        };
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

function browser({ coarse = false, readyState = 'complete' } = {}) {
    let now = 10_000;
    let frameId = 0;
    const frames = new Map();
    const window = new Events();
    const document = new Events();
    document.body = new Element('body');
    document.readyState = readyState;
    document.querySelectorAll = selector => document.body.querySelectorAll(selector);
    window.matchMedia = () => ({ matches: coarse });
    window.requestAnimationFrame = callback => {
        frames.set(++frameId, callback);
        return frameId;
    };
    const context = vm.createContext({ window, document, Element, Date: { now: () => now } });
    return {
        window,
        document,
        advance: milliseconds => { now += milliseconds; },
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
    };
}

function galleryFixture(options) {
    const env = browser(options);
    const gallery = new Element('div', { 'data-gallery': '' });
    const track = new Element('div', { 'data-gallery-track': '' });
    const current = new Element('span', { 'data-gallery-current': '' });
    const previous = new Element('button', { 'data-gallery-previous': '' });
    const next = new Element('button', { 'data-gallery-next': '' });
    const images = [];
    const slides = Array.from({ length: 5 }, (_, index) => {
        const slide = new Element('figure', { 'data-gallery-slide': '' });
        const image = new Element('img', { [index === 0 ? 'src' : 'data-src']: `/preview/${index}.jpg` });
        images.push(image);
        slide.offsetLeft = index * 300;
        return slide.append(image);
    });
    track.scrollLeft = 0;
    track.scrollTo = options => {
        track.lastScroll = { ...options };
        track.scrollLeft = options.left;
        track.emit('scroll');
    };
    track.append(...slides);
    gallery.append(track, previous, next, current);
    env.document.body.append(gallery);
    env.run('gallery.js');
    return { ...env, track, current, previous, next, images, slides };
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
    assert.equal('src' in env.images[1].dataset, false);
});

test('gallery navigation updates loading, counter and boundary buttons', () => {
    const env = galleryFixture();
    env.next.emit('click');
    assert.deepEqual(env.track.lastScroll, { left: 300, behavior: 'smooth' });
    assert.equal(env.images[2].src, '/preview/2.jpg');
    assert.equal(env.images[3].src, '');
    env.flushFrames();
    assert.equal(env.current.textContent, '2');
    assert.equal(env.previous.disabled, false);

    env.track.scrollLeft = 1190;
    env.track.emit('scroll');
    env.track.emit('scroll');
    assert.equal(env.flushFrames(), 1, 'scroll updates are coalesced into one frame');
    assert.equal(env.current.textContent, '5');
    assert.equal(env.next.disabled, true);
    assert.equal(env.images[3].src, '/preview/3.jpg');
    assert.equal(env.images[4].src, '/preview/4.jpg');

    env.previous.emit('click');
    env.flushFrames();
    assert.equal(env.current.textContent, '4');
    assert.equal(env.next.disabled, false);

    env.slides.forEach((slide, index) => { slide.offsetLeft = index * 400; });
    env.track.scrollLeft = 400;
    env.window.emit('resize');
    env.flushFrames();
    assert.equal(env.current.textContent, '2');
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
    assert.equal(env.tap(env.link, 'mouse').defaultPrevented, false);
    assert.equal(env.spoiler.classList.contains('spoiler_revealed'), false);
    assert.equal(env.tap(env.link, 'touch').defaultPrevented, true);

    const desktop = spoilerFixture({ coarse: false });
    assert.equal(desktop.document.emit('click', { target: desktop.link, detail: 1 }).defaultPrevented, false);
    assert.equal(desktop.tap(desktop.link, 'pen').defaultPrevented, true);
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
