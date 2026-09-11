# Haul

English | [繁體中文](README.zh-TW.md)

Paste a link, queue it, download it. Video, audio, images, streams — and every file is
verified to actually play before it is kept.

Comes as a desktop app (macOS Intel/Apple Silicon, Windows) and a CLI for scripts and
AI agents. Extraction is delegated to [yt-dlp](https://github.com/yt-dlp/yt-dlp)
(~1800 sites); Haul owns the queue, rate limiting, verification and file management.

## Install

Grab a build from [Releases](../../releases):

| Platform | App | CLI |
| --- | --- | --- |
| macOS | `Haul_x.y.z_universal.dmg` | `haul-universal-apple-darwin` |
| Windows | `Haul_x.y.z_x64-setup.exe` | `haul-x86_64-pc-windows-msvc.exe` |

The builds are not code-signed, so the first launch needs a one-time approval:

- **macOS** — System Settings → Privacy & Security → "Open Anyway"
- **Windows** — SmartScreen → "More info" → "Run anyway"

On first launch Haul downloads yt-dlp and ffmpeg (~80 MB) into its app folder. The app
and the CLI share one copy. From v0.2.0 the app checks GitHub Releases for updates and
installs them in place (Settings → "Check for Haul updates").

## App

Paste links (or drop them onto the window), pick **Video**, **Audio only** or **Images**,
and hit Add. Files land in `~/Downloads/Haul`.

- **Queue** and **Done** are two tabs. Every row shows a thumbnail (a video frame, the
  album cover, or the image itself) and the source URL — click the URL to copy it.
- A finished item **opens in the system player with one click**; hover for
  "Show in Finder". History survives restarts.
- Failed items offer **Retry**, and — when extraction is what failed — **Use browser**
  (a real Chrome loads the page and Haul intercepts the media request) or **Record**
  (capture the tab's video and audio when there is no file at all).
- Remove a row with **×**. Removing an unfinished item cancels the download; it asks first.
- Playlists, galleries and page scrapes are grouped under their source; click the source
  row to expand or collapse.
- ⚙ **Settings**: interface language (繁體中文 / English), output folder, quality cap,
  concurrency, login source (borrow your browser's cookies — Haul never touches
  passwords), browser path, recording limit, completion chime.

## CLI

```bash
haul <url>...                 # download video
haul -a <url>...              # audio only (original track, no re-encode)
haul -i <url>...              # images: video cover, a direct image, or every image on a page
haul -q 1080 <url>            # quality cap
haul -o <dir> <url>           # output folder
haul --any <url>              # also keep the page itself (refused by default, see below)
haul --cookies chrome <url>   # content that needs a login
haul --browser <url>          # let a real Chrome load the page when the first layers fail
haul record <url>             # record the tab's video + audio (-a for audio only)
haul status                   # history
haul logs                     # execution log (for diagnosing failures)
haul update                   # update yt-dlp
```

Playlists, channels and profiles expand into one item each. Plain files (images, PDFs,
archives) work too.

**The exit code is the answer** — that is the whole point of Haul over calling yt-dlp
directly:

| Code | Meaning |
| --- | --- |
| `0` | every item downloaded and passed the strongest check available for its type |
| `1` | at least one item failed |
| `2` | usage error, or yt-dlp / ffmpeg could not be prepared |

Each item reports which level of verification it passed, because the levels differ a lot:

| Level | Applies to | Check |
| --- | --- | --- |
| `media` | audio, video | full decode + silence detection + sampled frames |
| `image` | images | ffmpeg decodes one frame |
| `json` | JSON | actually parsed, so truncated responses are caught |
| `archive` | PDF, ZIP | magic bytes + trailing signature |
| `text` | plain text | valid non-empty UTF-8 |
| `integrity` | anything else | Content-Length only |

`text/html` is **not a file by default** — silently saving an HTML page when a video
extraction failed would be worse than failing. Pass `--any` to keep pages.

`--json` prints one NDJSON line per state change to stdout (human progress goes to stderr):

```bash
haul --json <url> | jq -r 'select(.status=="done") | .path'
```

### For AI agents

`.claude/skills/haul/` is a Claude Code skill that teaches an agent when to use Haul,
how to read the NDJSON, and what to do on failure (e.g. `haul update` then retry when a
site changed). Link it globally with:

```bash
ln -s "$PWD/.claude/skills/haul" ~/.claude/skills/haul
```

Logs live in `~/Library/Application Support/com.haul.desktop/logs/` (NDJSON, rotated),
and `haul status` reads `.haul-history.jsonl` in the output folder — no daemon, no port.

## How it works

```
link ─→ resolve ─→ expand list ─→ download each ─→ verify ─→ keep
           │                                          │
   yt-dlp → gallery-dl → direct → browser        delete on failure
```

Resolution is layered: yt-dlp for media sites; [gallery-dl](https://github.com/mikf/gallery-dl)
(optional, `pipx install gallery-dl`) for galleries; direct fetch for bare files, and
for image mode, any images found on an ordinary page; a real Chrome (optional) when the
media URL only exists inside a running page; recording as the last resort.

Verification decodes audio in-process (symphonia, falling back to ffmpeg for codecs it
lacks) and samples video frames at the start, middle and end with ffmpeg. A file that
fails is deleted and the reason is reported. Design notes live in `docs/plans/`.

## Build from source

Rust only — the UI is a single hand-written `ui/index.html`.

```bash
cargo build --release --workspace          # CLI at target/release/haul
cargo install tauri-cli --version "^2" --locked
cd src-tauri && cargo tauri build          # desktop installer
cargo test --workspace
```

Tests that need real network or media are gated behind environment variables
(`HAUL_TEST_MEDIA`, `HAUL_TEST_NET`, `HAUL_TEST_CHROME`) and skip otherwise.
