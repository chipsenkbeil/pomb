#!/usr/bin/env python3
"""Take a screenshot of ChipFlow via Tailscale proxy."""
import os
import sys
from urllib.parse import urlparse
from playwright.sync_api import sync_playwright

# Get proxy from environment (HTTPS_PROXY with port changed to 3130 for Tailscale)
https_proxy = os.environ.get('HTTPS_PROXY', '')
parsed = urlparse(https_proxy)
# Reconstruct with port 3130, keeping auth embedded in URL for Playwright
if parsed.username:
    auth_part = f"{parsed.username}:{parsed.password}@" if parsed.password else f"{parsed.username}@"
    tunnel_proxy = f"{parsed.scheme}://{auth_part}{parsed.hostname}:3130"
else:
    tunnel_proxy = f"{parsed.scheme}://{parsed.hostname}:3130"

url = sys.argv[1] if len(sys.argv) > 1 else 'https://chipflow.chip.network/'
output = sys.argv[2] if len(sys.argv) > 2 else '/tmp/chipflow-screenshot.png'

print(f"Proxy: {parsed.scheme}://***@{parsed.hostname}:3130", file=sys.stderr)
print(f"URL: {url}", file=sys.stderr)

proxy_config = {'server': tunnel_proxy}

with sync_playwright() as p:
    browser = p.chromium.launch(
        executable_path='/opt/meta-chromium/chrome',
        proxy=proxy_config,
        args=['--ignore-certificate-errors', '--no-sandbox']
    )
    page = browser.new_page(viewport={'width': 1920, 'height': 1080})
    try:
        page.goto(url, wait_until='networkidle', timeout=30000)
        page.wait_for_timeout(2000)
        page.screenshot(path=output, full_page=True)
        print(f"Screenshot saved to {output}", file=sys.stderr)
    except Exception as e:
        print(f"Error: {e}", file=sys.stderr)
        sys.exit(1)
    finally:
        browser.close()
