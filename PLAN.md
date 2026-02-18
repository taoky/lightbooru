# Plan

## Goal
Build a Rust project that provides a booru-like experience for gallery-dl downloads, with three UIs:
- TUI (ratatui)
- Web UI (no complex front-end framework)
- GUI (gtk4-rs + libadwaita)

Primary data source:
`~/Pictures/gallery-dl/<platform>/<id path>/xxx.jpg` and its `xxx.jpg.json` metadata.

## Metadata findings (8 sampled platforms)
Platforms sampled: bilibili, danbooru, mastodon, pixiv, tumblr, twitter, weibo, yandere.

Shared by all platforms (safe baseline):
- `category`
- `subcategory`
- `filename`
- `extension`

Common and meaningful for cross-platform indexing/filtering:
- Identity/time: `id`, `date`, `created_at`, `updated_at`
- Media/basic: `width`, `height`, `url`, `file_url`, `file_size`, `file_ext`, `md5`
- Content/search: `title`, `content`, `caption`, `description`, `tags`
- Social/moderation: `score`, `status`, `sensitive`
- Author/account: `author`, `user`, `username`, `account`, `blog_name`
- Thread/relationship: `parent_id`, `has_children`, `reblog`, `reblogged`, reply/quote/retweet IDs and counts

High-value platform-specific metadata to preserve in raw extras:
- bilibili: `detail`, `live_url`, `size`
- danbooru: split tag fields (`tags_*`, `tag_string_*`, `tag_count_*`), moderation flags, preview/large URLs
- mastodon: `spoiler_text`, `visibility`, `instance`, `mentions`, `media`, engagement counts
- pixiv: `total_view`, `total_bookmarks`, `total_comments`, `page_count`, `restrict`, `x_restrict`, `sanity_level`, `series`, `tools`
- tumblr: `post_url`, `summary`, `note_count`, `state`, `reblog_key`
- twitter: `tweet_id`, `conversation_id`, `favorite_count`, `retweet_count`, `reply_count`, `quote_count`, `view_count`, `hashtags`
- weibo: `status`, `cut_type`
- yandere: preview/sample/jpeg variants, `change`, moderation/lock flags, `frames*`

## High-level architecture
- **Core crate**: data model + indexing + storage + search + tagging + edit operations.
- **Ingest/index**: scan `~/Pictures/gallery-dl` (and configurable paths), parse metadata JSON, build an in-memory index on each startup (no DB).
- **Edits storage**: write user edits to a new sidecar `xxx.jpg.booru.json`; never modify original `xxx.jpg.json`.
- **Media access**: serve images and merged metadata (original + edits).
- **UI crates**: TUI, Web (read-only), GTK4/libadwaita reuse the core crate.
- **CLI**: `booructl` for inspect/edit/duplicate-finding.

## Steps
1. **Project bootstrap**
   - Create Cargo workspace with crates: `core`, `tui`, `web`, `gui`, and optional `cli`.
   - Add baseline dependencies (serde, serde_json, chrono, anyhow, thiserror, tracing).

2. **Data model + persistence**
   - Define `ImageItem`, `Metadata`, `Tag`, and `EditHistory`.
   - Normalize raw metadata into stable fields: `platform`, `post_id`, `author_name`, `posted_at`, `title`, `description`, `tags`, `sensitive`, `score`, `media_url`, `width`, `height`, `md5`.
   - Keep full original JSON as `raw_metadata` for platform-specific details and future migrations.
   - Metadata strategy: load original JSON + optional `*.booru.json`, merge into a view model.
   - Persistence: write only `*.booru.json` for edits (tags, sensitive, notes).

3. **Indexer / scanner**
   - Implement scanning of gallery-dl directory tree.
   - Parse `*.json` sidecars and link to image files.
   - Also parse `*.booru.json` edits if present.
   - Re-scan on startup (data size is small).

4. **Core operations**
   - Query/filter: by tags, sensitive, date, site/platform, etc.
   - Edit operations: add/remove tags, sensitive, notes.
   - Export or write-back strategy (optional) for metadata edits.

5. **TUI (ratatui)**
   - Search/filter panel, list view, detail view, preview panel.
   - Keybindings for tagging and sensitive flag.

6. **Web UI (simple, read-only)**
   - Serve a minimal HTTP server (axum or actix-web).
   - Render HTML via templates (askama, tera) and minimal JS.
   - Image grid + detail page + edit form.
   - Bind to localhost only (no auth).
   - No metadata editing endpoints.

7. **GTK4/libadwaita GUI**
   - Use an Adwaita-first app shell and responsive navigation:
     - `adw::Application` + `adw::ApplicationWindow`
     - `adw::ToolbarView` + `adw::HeaderBar` for title bar/buttons/search/actions
     - `adw::NavigationSplitView` as the primary layout (browser + detail/editor)
     - `adw::Breakpoint` rules to collapse/expand navigation on narrow screens
   - View switching and mode presentation:
     - `adw::ViewStack` for list/grid pages on the same dataset
     - `adw::ViewSwitcherTitle` for wide layouts
     - `adw::ViewSwitcherBar` for narrow layouts
   - Left browser panel supports two modes on the same dataset:
     - **List mode** (`gtk::ListView`) for dense text scanning
     - **Thumbnail mode** (`gtk::GridView`) for visual browsing
   - Shared model strategy for list/grid:
     - One data pipeline: `gio::ListStore -> gtk::FilterListModel -> gtk::SortListModel`
     - One shared `gtk::SingleSelection` so both modes keep the same selected item
     - Switching modes must preserve selection, filters, and sort state
   - Right detail/editor panel follows Adwaita preferences patterns:
     - Metadata summary (title/author/date/source URL)
     - Image preview (`gtk::Picture`)
     - Wrap editor content with `adw::Clamp` for readable width
     - `adw::PreferencesPage` + `adw::PreferencesGroup` for metadata/edit sections
     - `adw::EntryRow` for tags/notes and `adw::SwitchRow` for sensitive flag
     - Save writes `*.booru.json`
   - Feedback and state surfaces:
     - `adw::StatusPage` for empty/loading/error views
     - `adw::Banner` for recoverable errors/warnings
     - `adw::ToastOverlay` for non-blocking status feedback
   - App architecture inside `booru-gtk`:
     - `state`: app/query/filter/selection/view-mode state
     - `services`: thin wrappers around `booru-core` (`scan/search/edit`)
     - `widgets`: sidebar(list+grid), detail panel, filter bar, status surfaces
     - `actions/controller`: unidirectional event handling from UI to state updates
   - Visual style policy:
     - Follow system light/dark mode and accent colors
     - Prefer built-in Adwaita widgets/style classes; avoid heavy custom CSS
   - Thumbnail loading:
     - Asynchronous decode/resize to avoid UI stalls
     - Memory cache first, optional disk thumbnail cache later
     - Placeholder while loading/failing
   - Milestones:
     - M1: Adwaita shell + responsive split + status-page scaffold
     - M2: Search/filter + list mode + detail binding
     - M3: Add grid mode + mode switch + shared selection
     - M4: Add async thumbnail loader/cache + placeholders
     - M5: Tag/sensitive editing with save + toast/banner error flow
     - M6: Adwaita polish (keyboard nav, shortcuts, scroll restore, accessibility)

8. **CLI (`booructl`)**
   - View image info and merged metadata.
   - Edit metadata (write `*.booru.json`).
   - Find duplicates using perceptual hash (fuzzy hash) for images.

9. **Testing + packaging**
   - Unit tests for scanner/parser.
   - Integration tests for index and search.
   - Document config and usage in README.

## Confirmed constraints
- Do not edit original `xxx.jpg.json`; write edits to `xxx.jpg.booru.json`.
- Tags are user-editable (free-form).
- No database; scan on each startup; dataset is small.
- Linux-only for now.
- Web UI is localhost-only (no auth).
- Web UI is read-only; only TUI/GUI/CLI can edit metadata.
