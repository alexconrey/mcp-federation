(function() {
    // Determine current version from URL path
    var pathParts = window.location.pathname.split('/');
    // Path format: /mcp-federation/v0.1.0/chapter.html or /v0.1.0/chapter.html
    var currentVersion = 'latest';
    for (var i = 0; i < pathParts.length; i++) {
        if (pathParts[i] === 'latest' || pathParts[i].match(/^v\d/)) {
            currentVersion = pathParts[i];
            break;
        }
    }

    // Fetch versions.json from site root
    var baseUrl = window.location.pathname.split(currentVersion)[0] || '/';

    fetch(baseUrl + 'versions.json')
        .then(function(response) { return response.json(); })
        .then(function(versions) {
            if (!versions || versions.length === 0) return;

            var select = document.createElement('select');
            select.id = 'version-select';
            select.setAttribute('aria-label', 'Documentation version');

            versions.forEach(function(v) {
                var option = document.createElement('option');
                option.value = v.url;
                option.textContent = v.version;
                if (v.version === currentVersion) {
                    option.selected = true;
                }
                select.appendChild(option);
            });

            select.addEventListener('change', function() {
                var newUrl = baseUrl.replace(/\/$/, '') + this.value;
                window.location.href = newUrl;
            });

            // Insert into the mdBook right-side buttons area
            var nav = document.querySelector('.right-buttons');
            if (nav) {
                var wrapper = document.createElement('div');
                wrapper.className = 'version-select-wrapper';
                wrapper.appendChild(select);
                nav.insertBefore(wrapper, nav.firstChild);
            }
        })
        .catch(function(err) {
            console.log('Version switcher: could not load versions.json', err);
        });
})();
