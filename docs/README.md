# Documentation development

The site uses Zola. All pages inherit `templates/base.html` for shared styling.

Run a local preview from the repository root:

```sh
zola --root docs serve --port 8765 --output-dir /tmp/worksmith-docs-dev
```

Open <http://127.0.0.1:8765/>. `zola serve` sets the local base URL and rebuilds
on changes. A normal `zola build` uses the production URL in `zola.toml`, so
serving that output locally makes content links navigate to the live site.

To build and check a static preview instead:

```sh
zola --root docs build --base-url http://127.0.0.1:8765 --output-dir /tmp/worksmith-docs-preview --force
python3 docs/tests/check_site.py /tmp/worksmith-docs-preview http://127.0.0.1:8765
python3 -m http.server 8765 --bind 127.0.0.1 --directory /tmp/worksmith-docs-preview
```

Production builds keep the configured URL:

```sh
zola --root docs build
python3 docs/tests/check_site.py docs/public
```

CI checks both builds for internal links, anchors, shared styling, and text
contrast. The preview check also rejects links back to the production docs.
