"""Check built docs links and text contrast without network or extra packages.

Run after `zola build`: python3 docs/tests/check_site.py docs/public
"""

from html.parser import HTMLParser
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urljoin, urlsplit


def contrast(foreground, background):
    def luminance(color):
        rgb = [int(color[i:i + 2], 16) / 255 for i in (1, 3, 5)]
        linear = [v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4 for v in rgb]
        return sum(v * weight for v, weight in zip(linear, (0.2126, 0.7152, 0.0722)))

    light, dark = sorted((luminance(foreground), luminance(background)), reverse=True)
    return (light + 0.05) / (dark + 0.05)


class Page(HTMLParser):
    def __init__(self, source):
        super().__init__()
        self.ids = set()
        self.links = []
        self.images = []
        self.stack = []
        self.code_colors = []
        self.feed(source)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if 'id' in attrs:
            self.ids.add(attrs['id'])
        if tag == 'a' and 'href' in attrs:
            self.links.append(attrs['href'])
        if tag == 'img':
            self.images.append(attrs)
            if attrs.get('src'):
                self.links.append(attrs['src'])
        if tag in ('pre', 'code', 'span'):
            inherited = self.stack[-1][1].copy() if self.stack else {}
            inherited.update(dict(re.findall(r'(color|background-color):\s*(#[0-9A-Fa-f]{6})', attrs.get('style', ''))))
            self.stack.append((tag, inherited))

    def handle_endtag(self, tag):
        if self.stack and self.stack[-1][0] == tag:
            self.stack.pop()

    def handle_data(self, data):
        if data.strip() and self.stack:
            colors = self.stack[-1][1]
            if 'color' in colors and 'background-color' in colors:
                self.code_colors.append((colors['color'], colors['background-color']))


def check(root, base_url="https://worksmith.sh"):
    origin = urlsplit(base_url).netloc
    errors = []
    pages = {path.relative_to(root).as_posix(): Page(path.read_text()) for path in root.rglob('*.html')}
    assert 'quickstart/index.html' in pages, 'Build the docs before running these checks'
    reference_styles = re.findall(r'<style>(.*?)</style>', (root / 'quickstart/index.html').read_text(), re.S)
    for name, page in pages.items():
        if name != '404.html':
            styles = re.findall(r'<style>(.*?)</style>', (root / name).read_text(), re.S)
            if styles != reference_styles:
                errors.append(f'{name}: styling differs from the quickstart')
        url = base_url.rstrip('/') + '/' + name.removesuffix('index.html')
        for link in page.links:
            target = urlsplit(urljoin(url, link))
            if target.netloc == 'worksmith.sh' and origin != 'worksmith.sh':
                errors.append(f'{name}: preview link escapes to the live site: {link}')
            if target.netloc != origin or target.scheme not in ('http', 'https'):
                continue
            path = unquote(target.path).lstrip('/')
            if not path or path.endswith('/'):
                path += 'index.html'
            if not (root / path).is_file():
                errors.append(f'{name}: broken link {link}')
            elif target.fragment and path in pages and unquote(target.fragment) not in pages[path].ids:
                errors.append(f'{name}: missing anchor {link}')
        for img in page.images:
            if not img.get('src') or not img.get('alt', '').strip():
                errors.append(f'{name}: image needs a source and descriptive alternative text')
            if not all(img.get(dimension, '').isdigit() and int(img[dimension]) > 0 for dimension in ('width', 'height')):
                errors.append(f'{name}: image needs intrinsic dimensions to reserve layout space')
        for foreground, background in set(page.code_colors):
            if contrast(foreground, background) < 4.5:
                errors.append(f'{name}: code contrast below 4.5:1 ({foreground} on {background})')

    source = (root / 'quickstart/index.html').read_text()
    palettes = re.findall(r':root\s*\{([^}]+)\}', source)
    assert len(palettes) == 2, 'Expected light and dark palettes'
    for mode, block in zip(('light', 'dark'), palettes):
        colors = dict(re.findall(r'--([\w-]+):\s*(#[0-9a-fA-F]{6})', block))
        for foreground, background in [('text', 'background'), ('muted', 'background'), ('link', 'background'), ('code', 'surface')]:
            ratio = contrast(colors[foreground], colors[background])
            if ratio < 4.5:
                errors.append(f'{mode}: {foreground} on {background} has {ratio:.2f}:1 contrast')
    assert not errors, '\n'.join(errors)
    print(f'Checked {len(pages)} pages: shared styling, internal links, anchors, light/dark palettes, and syntax contrast pass.')


if __name__ == '__main__':
    check(Path(sys.argv[1] if len(sys.argv) > 1 else 'docs/public'),
          sys.argv[2] if len(sys.argv) > 2 else 'https://worksmith.sh')
