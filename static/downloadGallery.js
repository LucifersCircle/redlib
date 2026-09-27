// Usage: downloadGallery(images, baseName)

(function() {
    // Add js-enabled class for CSS detection
    document.body.classList.add('js-enabled');

    function extensionFor(blob, mediaUrl) {
        const mimeExtensions = {
            'image/avif': 'avif',
            'image/gif': 'gif',
            'image/jpeg': 'jpg',
            'image/png': 'png',
            'image/svg+xml': 'svg',
            'image/webp': 'webp',
            'video/mp4': 'mp4',
            'video/webm': 'webm',
        };
        const mime = (blob.type || '').split(';', 1)[0].toLowerCase();
        if (mimeExtensions[mime]) return mimeExtensions[mime];

        try {
            const match = new URL(mediaUrl, window.location.href).pathname.match(/\.([a-z0-9]{2,5})$/i);
            if (match) return match[1].toLowerCase();
        } catch (_) {
            // Use a neutral extension when the media URL cannot be parsed.
        }
        return 'bin';
    }

    window.downloadGallery = async function(images, baseName) {
        // Request permission to download multiple files
        // This will show a browser prompt asking for permission
        try {
            // Create a single reusable download link
            const a = document.createElement('a');
            a.style.display = 'none';
            document.body.appendChild(a);

            // Use the File System Access API if available, otherwise fall back to individual downloads
            for (let i = 0; i < images.length; i++) {
                const imageUrl = images[i];

                // Fetch the image as a blob
                const response = await fetch(imageUrl);
                if (!response.ok) throw new Error(`Media download failed with status ${response.status}`);
                const blob = await response.blob();
                const filename = `${baseName}_${i + 1}.${extensionFor(blob, imageUrl)}`;

                // Reuse the same link element
                const url = URL.createObjectURL(blob);
                a.href = url;
                a.download = filename;
                a.click();
                URL.revokeObjectURL(url);

                // Small delay between downloads to avoid overwhelming the browser
                if (i < images.length - 1) {
                    await new Promise(resolve => setTimeout(resolve, 100));
                }
            }

            document.body.removeChild(a);
        } catch (error) {
            console.error('Error downloading gallery:', error);
            alert('Failed to download some images. Please try downloading them individually.');
        }
    };

    // Attach click handlers to gallery download links
    function attachDownloadHandlers() {
        document.querySelectorAll('.download_gallery > a').forEach(function(link) {
            link.addEventListener('click', function(e) {
                e.preventDefault();

                // Find all gallery images
                const post = link.closest('.post');
                if (!post) return;

                const gallery = post.querySelector('.gallery');
                if (!gallery) return;

                const images = [];
                gallery.querySelectorAll('[data-gallery-original]').forEach(function(imageLink) {
                    images.push(imageLink.href);
                });

                if (images.length === 0) return;

                // Get base name from data attribute
                const baseName = link.getAttribute('data-base-name') || 'redlib_gallery';

                downloadGallery(images, baseName);
            });
        });
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', attachDownloadHandlers);
    } else {
        attachDownloadHandlers();
    }
})();
