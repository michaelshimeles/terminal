export const VERSION = '0.1.1';

/** `scripts/release-macos.sh` builds the release DMG as a universal binary. */
export const ARCH = 'Apple Silicon and Intel';
export const MIN_MACOS = '11';

export const NAME = 'Terminal';
/** `static/robots.txt` and `static/sitemap.xml` repeat this origin. */
export const SITE_URL = 'https://terminal.rasmic.xyz';
export const REPO_URL = 'https://github.com/michaelshimeles/terminal';
export const DOWNLOAD_URL = `${REPO_URL}/releases/latest/download/Terminal.dmg`;

export const TITLE = `${NAME}: a fast market terminal for the Mac`;
export const DESCRIPTION =
	'A fast, keyboard-driven market terminal for the Mac, written in Rust. GPU charts, tiled panes and a command line for stocks, crypto, FX, futures and options.';
export const OG_IMAGE_ALT = `${NAME}: the market, one command away. The app icon beside a screenshot of a watchlist, an AAPL chart and world markets, with AAPL typed into a command bar.`;
