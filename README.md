<img src="assets/icon.png" width="96" alt="Amber widget icon">

# Amber widget

Linux desktop dashboard and tray icon for Amber Electric prices and grid usage.

Download `amber-widget-linux-x86_64`, `icon.png` and `amber-widget.desktop` from the [latest release](https://github.com/joebywan/Amber-widget/releases/latest), or build with `cargo build --release`.

To install it, with the icon in your launcher and alt-tab on every desktop (Wayland needs this), run these from the download folder:

```bash
install -D amber-widget-linux-x86_64 ~/.local/bin/amber-widget
install -D -m 644 icon.png ~/.local/share/icons/hicolor/256x256/apps/amber-widget.png
install -D -m 644 amber-widget.desktop ~/.local/share/applications/amber-widget.desktop
```

To release, bump `version` in `Cargo.toml`. When that reaches `main`, the `build` workflow publishes it.
