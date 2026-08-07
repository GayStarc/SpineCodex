# SpineCodex project page

This branch hosts the standalone SpineCodex project page and launch film at
<https://ghabix.github.io/SpineCodex/>.

## Page behavior

- The launch film uses its final Agent Morphogenesis frame as the poster.
- Playback starts only after the visitor clicks play.
- A compact custom control bar provides playback, seeking, mute, and fullscreen
  controls without obscuring the film with browser-specific chrome.
- The install command is copied exactly as
  `npm install -g @spinejit/spine-codex@latest`.
- The GitHub icon links to the main SpineCodex repository.
- English and Simplified Chinese page copy, posters, and full films are
  supported. The initial language follows the browser locale, an explicit
  choice is saved locally, and switching languages preserves playback state.

## Media

- English video: `spinecodex-film-aria.mp4`
- Simplified Chinese video: `spinecodex-film-zh-cn.mp4`
- Posters: `spinecodex-film-poster.jpg` and
  `spinecodex-film-poster-zh-cn.jpg`
- Duration: 251.8 seconds
- Resolution: 1920x1080
- Video codec: H.264
- Audio codec: AAC
- Music: Kimiko Ishizaka, Open Goldberg Variations, Aria, CC0 1.0

The page also provides canonical, Open Graph, structured application metadata,
and a project sitemap for search discovery.
